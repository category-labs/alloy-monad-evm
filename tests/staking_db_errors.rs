//! Database error propagation through native and dynamic staking adapters.

use alloy_evm::{
    precompiles::{DynPrecompile, Precompile, PrecompileInput},
    EvmInternals,
};
use alloy_monad_evm::MonadPrecompilesMap;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use monad_revm::{
    monad_context_with_db,
    staking::{
        constants::{MON, SYSTEM_ADDRESS, UNIT_BIAS},
        storage::{
            accumulator_key, consensus_view_key, global_slots, snapshot_view_key, validator_key,
            validator_offsets, valset_slots, withdrawal_key, withdrawal_offsets,
        },
        STAKING_ADDRESS,
    },
    MonadBuilder, MonadCfgEnv, MonadHardfork,
};
use revm::{
    bytecode::opcode,
    context::{result::EVMError, TxEnv},
    context_interface::{ContextTr, JournalTr},
    database::InMemoryDB,
    database_interface::DBErrorMarker,
    handler::EvmTr,
    inspector::{InspectCommitEvm, NoOpInspector},
    precompile::{PrecompileError, PrecompileId, PrecompileStatus},
    state::{AccountInfo, Bytecode, EvmState},
    Database, DatabaseCommit, ExecuteCommitEvm,
};
use std::{fmt, sync::Arc};

const SENDER: Address = Address::repeat_byte(0x11);
const PROXY: Address = Address::repeat_byte(0x22);
const GAS_LIMIT: u64 = 2_000_000;

#[derive(Debug)]
struct DbFailure(Arc<()>);

impl fmt::Display for DbFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("staking database unavailable")
    }
}

impl std::error::Error for DbFailure {}
impl DBErrorMarker for DbFailure {}

#[derive(Debug, Default)]
struct FailingDb {
    inner: InMemoryDB,
    storage_failure: Option<(U256, DbFailure)>,
    account_failure: Option<(Address, DbFailure)>,
    commits: usize,
}

impl Database for FailingDb {
    type Error = DbFailure;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if self
            .account_failure
            .as_ref()
            .is_some_and(|(target, _)| *target == address)
        {
            return Err(self.account_failure.take().unwrap().1);
        }
        Ok(self.inner.basic(address).unwrap())
    }

    fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.inner.code_by_hash(hash).unwrap())
    }

    fn storage(&mut self, address: Address, key: U256) -> Result<U256, Self::Error> {
        if address == STAKING_ADDRESS
            && self
                .storage_failure
                .as_ref()
                .is_some_and(|(target, _)| *target == key)
        {
            return Err(self.storage_failure.take().unwrap().1);
        }
        Ok(self.inner.storage(address, key).unwrap())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.inner.block_hash(number).unwrap())
    }
}

impl DatabaseCommit for FailingDb {
    fn commit(&mut self, changes: EvmState) {
        self.commits += 1;
        self.inner.commit(changes);
    }
}

fn calldata(signature: &str, args: &[U256]) -> Bytes {
    let mut data = keccak256(signature).as_slice()[..4].to_vec();
    for arg in args {
        data.extend_from_slice(&arg.to_be_bytes::<32>());
    }
    data.into()
}

fn dynamic_precompiles(spec: MonadHardfork) -> MonadPrecompilesMap {
    let mut map = MonadPrecompilesMap::new_with_spec(spec);
    // Re-register the public stateful entry under a custom ID to exercise dynamic dispatch.
    map.apply_precompile(&STAKING_ADDRESS, |staking| {
        let staking = staking.unwrap();
        Some(DynPrecompile::new_stateful(
            PrecompileId::Custom("DynamicStaking".into()),
            move |input| staking.call(input),
        ))
    });
    map
}

fn catching_contract() -> Bytecode {
    Bytecode::new_raw(
        vec![
            // Forward calldata to staking, then write a marker if the call returns.
            opcode::CALLDATASIZE,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::CALLDATACOPY,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::CALLDATASIZE,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::PUSH2,
            0x10,
            0x00,
            opcode::PUSH3,
            0x0f,
            0x42,
            0x40,
            opcode::CALL,
            opcode::PUSH0,
            opcode::MSTORE,
            opcode::PUSH1,
            1,
            opcode::PUSH0,
            opcode::SSTORE,
            opcode::PUSH1,
            32,
            opcode::PUSH0,
            opcode::RETURN,
        ]
        .into(),
    )
}

#[derive(Clone, Copy)]
enum FailureSite {
    Getter,
    ReadAfterWrite,
    WriteAfterWrite,
}

fn assert_fatal_transaction(
    precompiles: MonadPrecompilesMap,
    spec: MonadHardfork,
    site: FailureSite,
    nested: bool,
    inspect: bool,
    typed_error: bool,
) {
    let proxy = if matches!(site, FailureSite::Getter) {
        PROXY
    } else {
        SYSTEM_ADDRESS
    };
    let sender = if nested || matches!(site, FailureSite::Getter) {
        SENDER
    } else {
        SYSTEM_ADDRESS
    };
    let mut db = FailingDb::default();
    db.inner.insert_account_info(
        sender,
        AccountInfo {
            balance: MON,
            ..Default::default()
        },
    );
    db.inner.insert_account_info(
        STAKING_ADDRESS,
        AccountInfo {
            nonce: 1,
            ..Default::default()
        },
    );
    if nested {
        db.inner
            .insert_account_info(proxy, AccountInfo::default().with_code(catching_contract()));
    }
    for (key, value) in [
        (valset_slots::CONSENSUS, U256::from(1) << 192),
        (
            valset_slots::CONSENSUS + U256::from(1),
            U256::from(1) << 192,
        ),
        (consensus_view_key(1, 0), MON),
        (consensus_view_key(1, 1), U256::from(123)),
    ] {
        db.inner
            .insert_account_storage(STAKING_ADDRESS, key, value)
            .unwrap();
    }
    let marker = Arc::new(());
    let key = match site {
        FailureSite::Getter => global_slots::EPOCH,
        FailureSite::ReadAfterWrite => valset_slots::CONSENSUS,
        FailureSite::WriteAfterWrite => snapshot_view_key(1, 1),
    };
    db.storage_failure = Some((key, DbFailure(marker.clone())));
    let signature = if matches!(site, FailureSite::Getter) {
        "getEpoch()"
    } else {
        "syscallSnapshot()"
    };
    let tx = TxEnv::builder()
        .caller(sender)
        .to(if nested { proxy } else { STAKING_ADDRESS })
        .data(calldata(signature, &[]))
        .gas_limit(GAS_LIMIT)
        .gas_price(1)
        .build_fill();
    let mut context = monad_context_with_db(db).with_cfg(MonadCfgEnv::new_with_spec(spec));
    context.block.basefee = 0;
    let mut evm = context
        .build_monad_with_inspector(NoOpInspector)
        .with_precompiles(precompiles);
    let error = if inspect {
        evm.inspect_tx_commit(tx.clone())
    } else {
        evm.transact_commit(tx.clone())
    }
    .expect_err("database failures must not produce a catchable EVM result");
    if typed_error {
        let EVMError::Database(DbFailure(original)) = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(Arc::ptr_eq(&original, &marker));
    } else {
        let EVMError::Custom(message) = error else {
            panic!("unexpected error: {error:?}");
        };
        assert_eq!(message, "fatal: staking database unavailable");
    }
    assert_eq!(evm.ctx().db().commits, 0);
    assert_eq!(evm.ctx().db().inner.cache.accounts[&sender].info.nonce, 0);
    assert_eq!(
        evm.ctx().db().inner.cache.accounts[&sender].info.balance,
        MON
    );
    assert_eq!(
        evm.ctx().db().inner.cache.accounts[&STAKING_ADDRESS]
            .storage
            .get(&global_slots::IN_BOUNDARY)
            .copied()
            .unwrap_or_default(),
        U256::ZERO
    );
    assert!(evm.transact_commit(tx).unwrap().is_success());
    assert_eq!(evm.ctx().db().commits, 1);
    assert_eq!(evm.ctx().db().inner.cache.accounts[&sender].info.nonce, 1);
}

#[test]
fn native_and_dynamic_database_errors_abort_transactions() {
    for spec in [
        MonadHardfork::MonadEight,
        MonadHardfork::MonadNine,
        MonadHardfork::MonadTen,
        MonadHardfork::MonadNext,
    ] {
        for site in [
            FailureSite::Getter,
            FailureSite::ReadAfterWrite,
            FailureSite::WriteAfterWrite,
        ] {
            for nested in [false, true] {
                for inspect in [false, true] {
                    assert_fatal_transaction(
                        MonadPrecompilesMap::new_with_spec(spec),
                        spec,
                        site,
                        nested,
                        inspect,
                        true,
                    );
                    assert_fatal_transaction(
                        dynamic_precompiles(spec),
                        spec,
                        site,
                        nested,
                        inspect,
                        false,
                    );
                }
            }
        }
    }
}

#[test]
fn dynamic_transfer_errors_remain_distinct_after_mutation() {
    for database_failure in [false, true] {
        let mut db = FailingDb::default();
        db.inner.insert_account_info(
            STAKING_ADDRESS,
            AccountInfo {
                balance: if database_failure { MON } else { U256::ZERO },
                nonce: 1,
                ..Default::default()
            },
        );
        for (key, value) in [
            (global_slots::EPOCH, U256::from(10) << 192),
            (
                withdrawal_key(1, &SENDER, 0, withdrawal_offsets::AMOUNT),
                U256::from(2),
            ),
            (
                withdrawal_key(1, &SENDER, 0, withdrawal_offsets::EPOCH),
                U256::from(1) << 192,
            ),
            (accumulator_key(1, 1, 0), UNIT_BIAS / U256::from(2)),
            (accumulator_key(1, 1, 1), U256::from(1)),
            (
                validator_key(1, validator_offsets::UNCLAIMED_REWARDS),
                U256::from(1),
            ),
        ] {
            db.inner
                .insert_account_storage(STAKING_ADDRESS, key, value)
                .unwrap();
        }
        if database_failure {
            db.account_failure = Some((SENDER, DbFailure(Arc::new(()))));
        }
        let mut context = monad_context_with_db(db);
        let map = dynamic_precompiles(MonadHardfork::MonadTen);
        let precompile = map.get(&STAKING_ADDRESS).unwrap();
        let data = calldata("withdraw(uint64,uint8)", &[U256::from(1), U256::ZERO]);
        let (block, tx, cfg, journal, _, _) = context.all_mut();
        let result = precompile.call(PrecompileInput {
            data: &data,
            gas: GAS_LIMIT,
            reservoir: 0,
            caller: SENDER,
            value: U256::ZERO,
            is_static: false,
            internals: EvmInternals::new(journal, block, cfg, tx),
            target_address: STAKING_ADDRESS,
            bytecode_address: STAKING_ADDRESS,
        });
        if database_failure {
            assert_eq!(
                result.unwrap_err(),
                PrecompileError::Fatal("staking database unavailable".into())
            );
        } else {
            let output = result.unwrap();
            assert_eq!(output.status, PrecompileStatus::Revert);
            assert_eq!(output.bytes.as_ref(), b"Transfer failed: OutOfFunds");
        }
        assert_eq!(
            context
                .journal_mut()
                .sload(STAKING_ADDRESS, accumulator_key(1, 1, 1))
                .unwrap()
                .data,
            U256::ZERO
        );
    }
}

#[test]
fn dynamic_staking_contract_errors_remain_catchable() {
    let mut db = FailingDb::default();
    db.inner.insert_account_info(
        SENDER,
        AccountInfo {
            balance: MON,
            ..Default::default()
        },
    );
    db.inner
        .insert_account_info(PROXY, AccountInfo::default().with_code(catching_contract()));
    let mut context = monad_context_with_db(db);
    context.block.basefee = 0;
    let mut evm = context
        .build_monad()
        .with_precompiles(dynamic_precompiles(MonadHardfork::MonadTen));
    let result = evm
        .transact_commit(
            TxEnv::builder()
                .caller(SENDER)
                .to(PROXY)
                .data(calldata("delegate(uint64)", &[U256::from(1)]))
                .gas_limit(GAS_LIMIT)
                .gas_price(1)
                .build_fill(),
        )
        .unwrap();
    assert!(result.is_success());
    assert_eq!(result.output().unwrap().as_ref(), &[0; 32]);
    assert_eq!(evm.ctx().db().commits, 1);
    assert_eq!(
        evm.ctx().db().inner.cache.accounts[&PROXY].storage[&U256::ZERO],
        U256::from(1)
    );
}
