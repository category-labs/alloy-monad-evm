//! Isolated execution must preserve the enclosing transaction's reserve boundary.

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_monad_evm::{MonadContext, MonadEvm, MonadEvmFactory, MonadHaltReason, MonadHandler};
use alloy_primitives::{Address, Bytes, U256};
use core::convert::Infallible;
use monad_revm::{MonadBuilder, MonadHardfork, MonadJournalTr};
use revm::{
    bytecode::opcode,
    context::{BlockEnv, CfgEnv, LocalContext, TxEnv},
    context_interface::{
        result::{EVMError, ExecutionResult},
        JournalTr,
    },
    database::InMemoryDB,
    handler::{EthFrame, FrameResult},
    inspector::InspectorHandler,
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInput, CallInputs,
        CallOutcome, FrameInput, InstructionResult, Interpreter, SharedMemory,
    },
    state::{AccountInfo, Bytecode},
    DatabaseCommit, Inspector,
};

const SENDER: Address = Address::repeat_byte(0x11);
const OUTER: Address = Address::repeat_byte(0x22);
const AUTHORITY: Address = Address::repeat_byte(0x33);
const IMPLEMENTATION: Address = Address::repeat_byte(0x44);
const RECIPIENT: Address = Address::repeat_byte(0x55);
const GAS_LIMIT: u64 = 1_000_000;

fn mon(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10).pow(U256::from(18))
}

#[derive(Debug, Default)]
struct DipObserver {
    saw_violation: bool,
}

impl Inspector<MonadContext<InMemoryDB>> for DipObserver {
    fn step_end(&mut self, _interp: &mut Interpreter, context: &mut MonadContext<InMemoryDB>) {
        self.saw_violation |= context.journaled_state.reserve_balance().has_violation();
    }
}

#[derive(Debug, Default)]
struct IsolatedCallInspector {
    calls: usize,
    saw_inner_violation: bool,
    inner_violation_at_return: bool,
    inner_result: Option<InstructionResult>,
}

impl Inspector<MonadContext<InMemoryDB>> for IsolatedCallInspector {
    fn call(
        &mut self,
        context: &mut MonadContext<InMemoryDB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        if inputs.target_address != AUTHORITY {
            return None;
        }
        self.calls += 1;
        let outer_depth = context.journaled_state.depth();
        assert_eq!(outer_depth, 1);
        assert!(context.journaled_state.reserve_balance().tracking_enabled());
        assert!(!context.journaled_state.preserves_reserve_balance_tracker());

        let mut inputs = inputs.clone();
        inputs.input = CallInput::Bytes(inputs.input.bytes(context));
        let mut isolated = context.clone();
        isolated.local = LocalContext::default();
        // An isolated root starts at depth zero, independently of the enclosing call stack.
        isolated.journaled_state.depth = 0;
        isolated
            .journaled_state
            .set_preserve_reserve_balance_tracker(true);
        let mut isolated = isolated.build_monad_with_inspector(DipObserver::default());
        let mut handler = MonadHandler::<_, EVMError<Infallible>, EthFrame<EthInterpreter>>::new();
        // Execute only the internal frame: the outer transaction owns fees and finalization.
        let result = handler
            .inspect_run_exec_loop(
                &mut isolated,
                FrameInit {
                    depth: 0,
                    memory: SharedMemory::new(),
                    frame_input: FrameInput::Call(Box::new(inputs)),
                },
            )
            .unwrap();
        let FrameResult::Call(outcome) = result else {
            panic!("isolated CALL must return a call outcome");
        };
        self.saw_inner_violation = isolated.0.inspector.saw_violation;
        self.inner_violation_at_return = isolated
            .0
            .ctx
            .journaled_state
            .reserve_balance()
            .has_violation();
        self.inner_result = Some(outcome.result.result);
        assert_eq!(isolated.0.ctx.journaled_state.depth(), 0);

        // Hand back the journal, including entries needed for outer rollback and its tracker.
        context.journaled_state = isolated.0.ctx.journaled_state;
        context.journaled_state.depth = outer_depth;
        context
            .journaled_state
            .set_preserve_reserve_balance_tracker(false);
        Some(outcome)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    Repay,
    LeaveDeficit,
    RevertInner,
}

fn append_call(code: &mut Vec<u8>, address: Address, value: U256) {
    code.extend_from_slice(&[opcode::PUSH0; 4]);
    code.push(opcode::PUSH32);
    code.extend_from_slice(&value.to_be_bytes::<32>());
    code.push(opcode::PUSH20);
    code.extend_from_slice(address.as_slice());
    code.extend_from_slice(&[opcode::GAS, opcode::CALL, opcode::POP]);
}

fn create_evm(completion: Completion) -> MonadEvm<InMemoryDB, IsolatedCallInspector> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: mon(12),
            ..Default::default()
        },
    );
    db.insert_account_info(
        AUTHORITY,
        AccountInfo {
            balance: mon(12),
            ..Default::default()
        }
        .with_code(Bytecode::new_eip7702(IMPLEMENTATION)),
    );
    let mut inner = vec![
        opcode::PUSH1,
        7,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::LOG0,
    ];
    append_call(&mut inner, RECIPIENT, mon(3));
    if completion == Completion::RevertInner {
        inner.extend_from_slice(&[opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
    } else {
        inner.push(opcode::STOP);
    }
    db.insert_account_info(
        IMPLEMENTATION,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(inner))),
    );
    let mut outer = vec![
        opcode::PUSH1,
        42,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::LOG0,
    ];
    append_call(&mut outer, AUTHORITY, U256::ZERO);
    if completion == Completion::Repay {
        // SELFDESTRUCT repays the authority without invoking its delegated code again.
        outer.push(opcode::PUSH20);
        outer.extend_from_slice(AUTHORITY.as_slice());
        outer.push(opcode::SELFDESTRUCT);
    } else {
        outer.push(opcode::STOP);
    }
    db.insert_account_info(
        OUTER,
        AccountInfo {
            balance: mon(3),
            ..Default::default()
        }
        .with_code(Bytecode::new_raw(Bytes::from(outer))),
    );
    MonadEvmFactory::default().create_evm_with_inspector(
        db,
        EvmEnv::new(
            CfgEnv::new_with_spec(MonadHardfork::MonadTen),
            BlockEnv {
                basefee: 0,
                ..Default::default()
            },
        ),
        IsolatedCallInspector::default(),
    )
}

#[test]
fn preserved_isolated_calls_are_adjudicated_at_the_enclosing_transaction_boundary() {
    for completion in [
        Completion::Repay,
        Completion::LeaveDeficit,
        Completion::RevertInner,
    ] {
        let mut evm = create_evm(completion);
        let out = evm
            .transact(
                TxEnv::builder()
                    .caller(SENDER)
                    .to(OUTER)
                    .gas_limit(GAS_LIMIT)
                    .gas_price(1)
                    .build_fill(),
            )
            .unwrap();
        let inspector = evm.components().1;
        assert_eq!(inspector.calls, 1);
        assert!(inspector.saw_inner_violation);
        assert_eq!(
            inspector.inner_violation_at_return,
            completion != Completion::RevertInner
        );
        assert_eq!(
            inspector.inner_result,
            Some(if completion == Completion::RevertInner {
                InstructionResult::Revert
            } else {
                InstructionResult::Stop
            }),
        );

        if completion == Completion::LeaveDeficit {
            assert!(matches!(
                out.result,
                ExecutionResult::Halt {
                    reason: MonadHaltReason::ReserveBalanceViolation,
                    ..
                }
            ));
            assert!(out.result.logs().is_empty());
        } else {
            assert!(out.result.is_success(), "{completion:?}: {:?}", out.result);
            assert_eq!(
                out.result.logs().len(),
                if completion == Completion::Repay {
                    2
                } else {
                    1
                }
            );
        }
        assert_eq!(out.state[&SENDER].info.nonce, 1);
        assert_eq!(
            out.state[&SENDER].info.balance,
            mon(12) - U256::from(GAS_LIMIT)
        );
        assert_eq!(out.state[&AUTHORITY].info.balance, mon(12));
        assert_eq!(
            out.state[&AUTHORITY].storage[&U256::ZERO].present_value(),
            if completion == Completion::Repay {
                U256::from(7)
            } else {
                U256::ZERO
            },
        );
        assert_eq!(
            out.state[&OUTER].storage[&U256::ZERO].present_value(),
            if completion == Completion::LeaveDeficit {
                U256::ZERO
            } else {
                U256::from(42)
            },
        );
        assert_eq!(
            out.state[&RECIPIENT].info.balance,
            if completion == Completion::Repay {
                mon(3)
            } else {
                U256::ZERO
            },
        );
        assert_eq!(
            out.state[&OUTER].info.balance,
            if completion == Completion::Repay {
                U256::ZERO
            } else {
                mon(3)
            },
        );
        assert!(!evm
            .ctx()
            .journaled_state
            .preserves_reserve_balance_tracker());
        assert!(!evm
            .ctx()
            .journaled_state
            .reserve_balance()
            .tracking_enabled());
        assert!(evm.ctx().journaled_state.logs.is_empty());

        evm.db_mut().commit(out.state);
        evm.ctx_mut()
            .chain
            .parent_senders_and_authorities
            .insert(SENDER);
        let next = evm
            .transact(
                TxEnv::builder()
                    .caller(SENDER)
                    .to(RECIPIENT)
                    .value(mon(3))
                    .nonce(1)
                    .gas_limit(GAS_LIMIT)
                    .gas_price(1)
                    .build_fill(),
            )
            .unwrap();
        assert!(matches!(
            next.result,
            ExecutionResult::Halt {
                reason: MonadHaltReason::ReserveBalanceViolation,
                ..
            }
        ));
        assert_eq!(next.state[&SENDER].info.nonce, 2);
        assert_eq!(
            next.state[&SENDER].info.balance,
            mon(12) - U256::from(2 * GAS_LIMIT)
        );
        assert_eq!(evm.components().1.calls, 1);
        assert!(!evm
            .ctx()
            .journaled_state
            .reserve_balance()
            .tracking_enabled());
    }
}
