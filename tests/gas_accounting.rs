//! Charged gas in Alloy receipts and block limits remains separate from execution consumption.

use alloy_consensus::{
    transaction::Recovered, SignableTransaction, TxEnvelope, TxLegacy, TxReceipt,
};
use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    eth::{
        receipt_builder::AlloyReceiptBuilder, spec::EthSpec, EthBlockExecutionCtx, EthBlockExecutor,
    },
    Evm, EvmEnv, EvmFactory,
};
use alloy_monad_evm::{MonadContext, MonadEvmFactory};
use alloy_primitives::{Address, Bytes, Signature, TxKind, B256, U256};
use monad_revm::MonadHardfork;
use revm::{
    bytecode::opcode,
    context::{BlockEnv, CfgEnv},
    context_interface::result::ExecutionResult,
    database::InMemoryDB,
    interpreter::{CallInputs, CallOutcome},
    state::{AccountInfo, Bytecode},
    Inspector,
};

const SENDER: Address = Address::repeat_byte(0x11);
const SUCCESS: Address = Address::repeat_byte(0x22);
const REVERT: Address = Address::repeat_byte(0x33);
const HALT: Address = Address::repeat_byte(0x44);
const BENEFICIARY: Address = Address::repeat_byte(0x55);
const INITIAL_BALANCE: u64 = 5_000_000;
const GAS_PRICE: u64 = 7;
const BASE_FEE: u64 = 2;

#[derive(Debug, Default)]
struct RemainingGasInspector {
    remaining: Vec<u64>,
}

impl Inspector<MonadContext<InMemoryDB>> for RemainingGasInspector {
    fn call_end(
        &mut self,
        _context: &mut MonadContext<InMemoryDB>,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.remaining.push(outcome.result.gas.remaining());
    }
}

fn transaction(nonce: u64, target: Address, gas_limit: u64) -> Recovered<TxEnvelope> {
    let tx = TxLegacy {
        nonce,
        gas_price: u128::from(GAS_PRICE),
        gas_limit,
        to: TxKind::Call(target),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();
    Recovered::new_unchecked(tx, SENDER)
}

#[test]
fn block_receipts_and_capacity_use_charged_transaction_gas() {
    for spec in [
        MonadHardfork::MonadEight,
        MonadHardfork::MonadNine,
        MonadHardfork::MonadTen,
        MonadHardfork::MonadNext,
    ] {
        for inspect in [false, true] {
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                SENDER,
                AccountInfo {
                    balance: U256::from(INITIAL_BALANCE),
                    ..Default::default()
                },
            );
            for (address, code) in [
                (SUCCESS, vec![opcode::STOP]),
                (REVERT, vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT]),
                (HALT, vec![opcode::INVALID]),
            ] {
                db.insert_account_info(
                    address,
                    AccountInfo::default().with_code(Bytecode::new_raw(code.into())),
                );
            }
            let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                db,
                EvmEnv::new(
                    CfgEnv::new_with_spec(spec),
                    BlockEnv {
                        number: U256::from(20_000_000),
                        timestamp: U256::from(1_700_000_000),
                        gas_limit: 400_000,
                        beneficiary: BENEFICIARY,
                        basefee: BASE_FEE,
                        ..Default::default()
                    },
                ),
                RemainingGasInspector::default(),
            );
            evm.set_inspector_enabled(inspect);
            let mut executor = EthBlockExecutor::new(
                evm,
                EthBlockExecutionCtx {
                    parent_hash: B256::ZERO,
                    parent_beacon_block_root: None,
                    ommers: &[],
                    withdrawals: None,
                    extra_data: Bytes::new(),
                    tx_count_hint: Some(3),
                    slot_number: None,
                },
                EthSpec::mainnet(),
                AlloyReceiptBuilder::default(),
            );
            executor.apply_pre_execution_changes().unwrap();

            for (nonce, (target, gas_limit, consumed, cumulative)) in [
                (SUCCESS, 100_000, 21_000, 100_000),
                (REVERT, 120_000, 21_004, 220_000),
                (HALT, 140_000, 140_000, 360_000),
            ]
            .into_iter()
            .enumerate()
            {
                let output = executor
                    .execute_transaction_without_commit(transaction(
                        nonce as u64,
                        target,
                        gas_limit,
                    ))
                    .unwrap();
                match (&output.result.result, nonce) {
                    (ExecutionResult::Success { .. }, 0)
                    | (ExecutionResult::Revert { .. }, 1)
                    | (ExecutionResult::Halt { .. }, 2) => {}
                    _ => panic!("unexpected result: {:?}", output.result.result),
                }
                let gas = output.result.result.gas();
                assert_eq!(gas.tx_gas_used(), gas_limit);
                assert_eq!(gas.block_regular_gas_used(), gas_limit);
                assert_eq!(gas.block_state_gas_used(), 0);
                assert_eq!(gas.total_gas_spent(), consumed);
                assert_eq!(gas.final_refunded(), 0);
                if inspect && nonce < 2 {
                    assert_eq!(
                        executor.evm.components().1.remaining.last(),
                        Some(&(gas_limit - consumed))
                    );
                }

                let gas_output = executor.commit_transaction(output);
                assert_eq!(gas_output.tx_gas_used(), gas_limit);
                assert_eq!(gas_output.state_gas_used(), 0);
                assert_eq!(executor.cumulative_tx_gas_used, cumulative);
                assert_eq!(executor.block_regular_gas_used, cumulative);
                assert_eq!(executor.block_state_gas_used, 0);
                let receipt = &executor.receipts()[nonce];
                assert_eq!(receipt.cumulative_gas_used(), cumulative);
                assert_eq!(receipt.status(), nonce == 0);
                assert!(receipt.logs().is_empty());

                let db = executor.evm.db();
                assert_eq!(db.cache.accounts[&SENDER].info.nonce, nonce as u64 + 1);
                assert_eq!(
                    db.cache.accounts[&SENDER].info.balance,
                    U256::from(INITIAL_BALANCE - cumulative * GAS_PRICE)
                );
                assert_eq!(
                    db.cache.accounts[&BENEFICIARY].info.balance,
                    U256::from(cumulative * (GAS_PRICE - BASE_FEE))
                );
            }

            // Block admission must account for charges already included in receipts.
            let error = executor
                .execute_transaction(transaction(3, SUCCESS, 100_000))
                .unwrap_err();
            assert!(matches!(
                error,
                BlockExecutionError::Validation(
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                        transaction_gas_limit: 100_000,
                        block_available_gas: 40_000,
                    }
                )
            ));
            assert_eq!(executor.receipts().len(), 3);
            assert_eq!(executor.evm.db().cache.accounts[&SENDER].info.nonce, 3);

            let (evm, block) = executor.finish().unwrap();
            assert_eq!(block.gas_used, 360_000);
            assert_eq!(block.blob_gas_used, 0);
            assert_eq!(
                block
                    .receipts
                    .iter()
                    .map(TxReceipt::cumulative_gas_used)
                    .collect::<Vec<_>>(),
                [100_000, 220_000, 360_000]
            );
            if !inspect {
                assert!(evm.components().1.remaining.is_empty());
            }
        }
    }
}
