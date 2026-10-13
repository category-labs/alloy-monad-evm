//! Stock transaction traces must observe reserve rejection before root completion.

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_monad_evm::{MonadEvmFactory, MonadHaltReason};
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_trace::geth::CallConfig;
use monad_revm::MonadHardfork;
use revm::{
    bytecode::opcode,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::{ExecutionResult, ResultAndState},
    database::InMemoryDB,
    interpreter::InstructionResult,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const GAS_LIMIT: u64 = 1_000_000;

fn mon(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10).pow(U256::from(18))
}

fn assert_rejected_trace(
    inspector: &TracingInspector,
    out: &ResultAndState<MonadHaltReason>,
    create: bool,
    children: usize,
    attempted_root_logs: usize,
) {
    assert!(matches!(
        out.result,
        ExecutionResult::Halt {
            reason: MonadHaltReason::ReserveBalanceViolation,
            ..
        }
    ));
    let traces = inspector.traces().nodes();
    assert_eq!(traces.len(), children + 1);
    assert_eq!(traces[0].trace.status, Some(InstructionResult::Revert));
    assert!(!traces[0].trace.success);
    assert_eq!(traces[0].trace.gas_refund_counter, 0);
    assert_eq!(
        traces[0].trace.gas_limit - traces[0].trace.gas_used,
        GAS_LIMIT - out.result.gas().total_gas_spent()
    );

    let geth = inspector.geth_builder();
    let calls = geth.geth_call_traces_with_result_gas(
        CallConfig {
            with_log: Some(true),
            ..Default::default()
        },
        *out.result.gas(),
    );
    assert_eq!(calls.error.as_deref(), Some("execution reverted"));
    // The stock tracer retains attempted root logs, even when execution is reverted.
    assert_eq!(calls.logs.len(), attempted_root_logs);
    assert_eq!(calls.calls.len(), children);
    for child in &calls.calls {
        assert!(child.error.is_none());
        assert!(child.logs.is_empty());
    }
    if create {
        assert!(calls.to.is_none());
        assert_eq!(out.result.gas().total_gas_spent(), GAS_LIMIT);
    }
    let opcodes =
        geth.geth_traces_with_result_gas(*out.result.gas(), Bytes::new(), Default::default());
    assert!(opcodes.failed);

    let parity = inspector
        .clone()
        .into_parity_builder()
        .into_transaction_traces();
    assert_eq!(parity.len(), children + 1);
    assert_eq!(parity[0].error.as_deref(), Some("Reverted"));
    assert_eq!(parity[0].subtraces, children);
    assert!(parity[0].trace_address.is_empty());
    for (index, child) in parity.iter().skip(1).enumerate() {
        assert!(child.error.is_none());
        assert_eq!(child.trace_address, vec![index]);
    }
    assert_eq!(out.state[&SENDER].info.nonce, 1);
    assert_eq!(
        out.state[&SENDER].info.balance,
        mon(12) - U256::from(GAS_LIMIT)
    );
    assert!(out.result.logs().is_empty());
}

#[test]
fn stock_geth_and_parity_traces_report_rejected_call_and_create() {
    for spec in [
        MonadHardfork::MonadEight,
        MonadHardfork::MonadNine,
        MonadHardfork::MonadTen,
        MonadHardfork::MonadNext,
    ] {
        for create in [false, true] {
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                SENDER,
                AccountInfo {
                    balance: mon(12),
                    ..Default::default()
                },
            );
            db.insert_account_info(
                CHILD,
                AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
                    opcode::PUSH0,
                    opcode::PUSH0,
                    opcode::LOG0,
                ]))),
            );
            let mut code = vec![
                opcode::PUSH1,
                42,
                opcode::PUSH0,
                opcode::SSTORE,
                opcode::PUSH0,
                opcode::PUSH0,
                opcode::LOG0,
            ];
            code.extend_from_slice(&[opcode::PUSH0; 5]);
            code.push(opcode::PUSH20);
            code.extend_from_slice(CHILD.as_slice());
            code.extend_from_slice(&[
                opcode::GAS,
                opcode::CALL,
                opcode::POP,
                opcode::PUSH1,
                1,
                opcode::PUSH0,
                opcode::RETURN,
            ]);
            if !create {
                db.insert_account_info(
                    TARGET,
                    AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(code.clone()))),
                );
            }
            let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                db,
                EvmEnv::new(
                    CfgEnv::new_with_spec(spec),
                    BlockEnv {
                        basefee: 0,
                        ..Default::default()
                    },
                ),
                TracingInspector::new(TracingInspectorConfig::all()),
            );
            evm.ctx_mut()
                .chain
                .parent_senders_and_authorities
                .insert(SENDER);
            let tx = TxEnv::builder()
                .caller(SENDER)
                .kind(if create {
                    TxKind::Create
                } else {
                    TxKind::Call(TARGET)
                })
                .data(if create {
                    Bytes::from(code)
                } else {
                    Bytes::new()
                })
                .value(mon(3))
                .gas_limit(GAS_LIMIT)
                .gas_price(1)
                .build_fill();
            let out = evm.transact(tx).unwrap();
            assert_rejected_trace(evm.components().1, &out, create, 1, 1);
            let target = if create { SENDER.create(0) } else { TARGET };
            assert_eq!(out.state[&target].info.balance, U256::ZERO);
            assert!(out.state[&target]
                .storage
                .values()
                .all(|slot| slot.present_value() == U256::ZERO));
        }
    }
}

#[test]
fn stock_traces_report_rejected_immediate_call_results() {
    for recipient in [TARGET, Address::with_last_byte(4)] {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SENDER,
            AccountInfo {
                balance: mon(12),
                ..Default::default()
            },
        );
        let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
            db,
            EvmEnv::new(
                CfgEnv::new_with_spec(MonadHardfork::MonadTen),
                BlockEnv {
                    basefee: 0,
                    ..Default::default()
                },
            ),
            TracingInspector::new(TracingInspectorConfig::all()),
        );
        evm.ctx_mut()
            .chain
            .parent_senders_and_authorities
            .insert(SENDER);
        let tx = TxEnv::builder()
            .caller(SENDER)
            .to(recipient)
            .value(mon(3))
            .gas_limit(GAS_LIMIT)
            .gas_price(1)
            .build_fill();
        let out = evm.transact(tx).unwrap();
        assert_rejected_trace(evm.components().1, &out, false, 0, 0);
        assert_eq!(out.state[&recipient].info.balance, U256::ZERO);
    }
}

#[test]
fn ordinary_revert_retains_attempted_trace_logs_but_no_committed_logs() {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: mon(12),
            ..Default::default()
        },
    );
    db.insert_account_info(
        CHILD,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::LOG0,
            opcode::STOP,
        ]))),
    );
    let mut code = vec![opcode::PUSH0, opcode::PUSH0, opcode::LOG0];
    code.extend_from_slice(&[opcode::PUSH0; 5]);
    code.push(opcode::PUSH20);
    code.extend_from_slice(CHILD.as_slice());
    code.extend_from_slice(&[
        opcode::GAS,
        opcode::CALL,
        opcode::POP,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::REVERT,
    ]);
    db.insert_account_info(
        TARGET,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(code))),
    );
    let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
        db,
        EvmEnv::new(
            CfgEnv::new_with_spec(MonadHardfork::MonadTen),
            BlockEnv {
                basefee: 0,
                ..Default::default()
            },
        ),
        TracingInspector::new(TracingInspectorConfig::all()),
    );
    let out = evm
        .transact(
            TxEnv::builder()
                .caller(SENDER)
                .to(TARGET)
                .gas_limit(GAS_LIMIT)
                .gas_price(1)
                .build_fill(),
        )
        .unwrap();
    assert!(matches!(out.result, ExecutionResult::Revert { .. }));
    assert!(out.result.logs().is_empty());
    assert!(evm.ctx().journaled_state.logs.is_empty());
    let inspector = evm.components().1;
    let traces = inspector.traces().nodes();
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[0].trace.status, Some(InstructionResult::Revert));
    assert_eq!(traces[0].logs.len(), 1);
    assert_eq!(traces[1].logs.len(), 1);
    let calls = inspector.geth_builder().geth_call_traces_with_result_gas(
        CallConfig {
            with_log: Some(true),
            ..Default::default()
        },
        *out.result.gas(),
    );
    assert_eq!(calls.error.as_deref(), Some("execution reverted"));
    assert_eq!(calls.logs.len(), 1);
    assert_eq!(calls.calls.len(), 1);
    assert!(calls.calls[0].logs.is_empty());
    assert!(calls.calls[0].error.is_none());
    let parity = inspector
        .clone()
        .into_parity_builder()
        .into_transaction_traces();
    assert_eq!(parity[0].error.as_deref(), Some("Reverted"));
    assert_eq!(parity[1].trace_address, vec![0]);
    assert!(parity[1].error.is_none());
}
