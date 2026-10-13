//! Reserve rejection through the Alloy factory and EVM traits.

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_monad_evm::{MonadEvmFactory, MonadHaltReason};
use alloy_primitives::{Address, U256};
use monad_revm::MonadHardfork;
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::ExecutionResult,
    database::InMemoryDB,
    primitives::TxKind,
    state::AccountInfo,
};

#[test]
fn factory_preserves_reserve_rejection_reason_and_transaction_effects() {
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    let mon = U256::from(10).pow(U256::from(18));
    for spec in [
        MonadHardfork::MonadEight,
        MonadHardfork::MonadNine,
        MonadHardfork::MonadTen,
        MonadHardfork::MonadNext,
    ] {
        for create in [false, true] {
            for inspect in [false, true] {
                let mut db = InMemoryDB::default();
                db.insert_account_info(
                    sender,
                    AccountInfo {
                        balance: mon * U256::from(12),
                        ..Default::default()
                    },
                );
                let mut evm = MonadEvmFactory::default().create_evm(
                    db,
                    EvmEnv::new(
                        CfgEnv::new_with_spec(spec),
                        BlockEnv {
                            basefee: 0,
                            ..Default::default()
                        },
                    ),
                );
                evm.ctx_mut()
                    .chain
                    .parent_senders_and_authorities
                    .insert(sender);
                evm.set_inspector_enabled(inspect);
                let tx = TxEnv::builder()
                    .caller(sender)
                    .kind(if create {
                        TxKind::Create
                    } else {
                        TxKind::Call(recipient)
                    })
                    .value(mon * U256::from(3))
                    .gas_limit(100_000)
                    .gas_price(1)
                    .build_fill();
                let out = evm.transact(tx).unwrap();
                assert!(matches!(
                    out.result,
                    ExecutionResult::Halt {
                        reason: MonadHaltReason::ReserveBalanceViolation,
                        ..
                    }
                ));
                assert_eq!(
                    out.state[&sender].info.balance,
                    mon * U256::from(12) - U256::from(100_000)
                );
                assert_eq!(out.state[&sender].info.nonce, 1);
                let target = if create { sender.create(0) } else { recipient };
                assert!(out
                    .state
                    .get(&target)
                    .is_none_or(|account| account.info.balance.is_zero()));
            }
        }
    }
}
