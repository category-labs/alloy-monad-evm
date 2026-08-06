#![cfg_attr(not(test), warn(unused_crate_dependencies))]

//! Alloy EVM implementation for Monad blockchain.
//!
//! This crate provides:
//! - [`MonadEvm`]: Wrapper implementing [`alloy_evm::Evm`] trait
//! - [`MonadEvmFactory`]: Factory implementing [`alloy_evm::EvmFactory`] trait
//! - [`MonadContext`]: Type alias for Monad EVM context (re-exported from monad-revm)
//! - [`extend_monad_precompiles_for_spec`]: Function to extend `PrecompilesMap` with Monad precompiles

use alloy_evm::{
    precompiles::{DynPrecompile, Precompile, PrecompileInput, PrecompilesMap},
    Database, Evm, EvmEnv, EvmFactory, EvmInternals,
};
use alloy_primitives::{Address, Bytes, U256};
use monad_revm::{
    instructions::MonadInstructions,
    monad_context_with_db,
    precompiles::MonadPrecompiles,
    reserve_balance::{self, abi::RESERVE_BALANCE_ADDRESS},
    staking::{self, write::StakingStorage, StorageReader, STAKING_ADDRESS},
    MonadBuilder, MonadCfgEnv, MonadEvm as InnerMonadEvm, MonadHardfork,
};
use revm::{
    context::{BlockEnv, CfgEnv, DBErrorMarker, TxEnv},
    context_interface::result::{EVMError, HaltReason, ResultAndState},
    context_interface::{ContextTr, JournalTr, LocalContextTr},
    handler::{precompile_output_to_interpreter_result, PrecompileProvider},
    inspector::{InspectSystemCallEvm, NoOpInspector},
    interpreter::{CallInputs, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput},
    primitives::AddressSet,
    Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, OnceLock,
    },
};

// Re-export monad-revm types for external users
pub use monad_revm::{handler::MonadHandler, MonadContext};

const MONAD_STAKING_ID: &str = "MonadStaking";
const MONAD_RESERVE_BALANCE_ID: &str = "MonadReserveBalance";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReserveBalanceManagement {
    Managed,
    Overridden,
    Removed,
}

fn is_native_monad_id(id: &PrecompileId, expected: &str) -> bool {
    matches!(id, PrecompileId::Custom(name) if name.as_ref() == expected)
}

fn static_monad_precompiles(spec: MonadHardfork) -> &'static revm::precompile::Precompiles {
    static MONAD_EIGHT: OnceLock<&'static revm::precompile::Precompiles> = OnceLock::new();
    static MONAD_NINE: OnceLock<&'static revm::precompile::Precompiles> = OnceLock::new();
    static MONAD_NEXT: OnceLock<&'static revm::precompile::Precompiles> = OnceLock::new();

    let precompiles = match spec {
        MonadHardfork::MonadEight => &MONAD_EIGHT,
        MonadHardfork::MonadNine => &MONAD_NINE,
        MonadHardfork::MonadNext => &MONAD_NEXT,
    };
    precompiles.get_or_init(|| MonadPrecompiles::new_with_spec(spec).precompiles())
}

fn runtime_monad_precompiles(runtime_spec: Arc<AtomicU8>) -> PrecompilesMap {
    let monad_eight = static_monad_precompiles(MonadHardfork::MonadEight);
    let monad_nine = static_monad_precompiles(MonadHardfork::MonadNine);
    let monad_next = static_monad_precompiles(MonadHardfork::MonadNext);

    let same_addresses = |other: &'static revm::precompile::Precompiles| {
        monad_eight
            .addresses()
            .all(|address| other.contains(address))
            && other
                .addresses()
                .all(|address| monad_eight.contains(address))
    };
    assert!(
        same_addresses(monad_nine) && same_addresses(monad_next),
        "runtime Monad precompile selection requires stable protocol addresses"
    );

    let mut precompiles = PrecompilesMap::from_static(monad_eight);
    for address in monad_eight.addresses().copied().collect::<Vec<_>>() {
        let monad_eight = monad_eight
            .get(&address)
            .expect("MonadEight precompile address should be present");
        let monad_nine = monad_nine
            .get(&address)
            .expect("MonadNine precompile address should be present");
        let monad_next = monad_next
            .get(&address)
            .expect("MonadNext precompile address should be present");
        assert!(
            monad_eight.id() == monad_nine.id() && monad_eight.id() == monad_next.id(),
            "runtime Monad precompile selection requires stable protocol identifiers"
        );
        let id = monad_eight.id().clone();
        let runtime_spec = Arc::clone(&runtime_spec);
        precompiles.apply_precompile(&address, |_| {
            Some(DynPrecompile::new_stateful(id, move |input| {
                let precompile = match runtime_spec.load(Ordering::Relaxed) {
                    spec if spec == MonadHardfork::MonadEight as u8 => monad_eight,
                    spec if spec == MonadHardfork::MonadNine as u8 => monad_nine,
                    spec if spec == MonadHardfork::MonadNext as u8 => monad_next,
                    spec => unreachable!("invalid runtime Monad hardfork discriminant: {spec}"),
                };
                precompile.execute(input.data, input.gas, input.reservoir)
            }))
        });
    }
    precompiles
}

/// Monad-aware precompile wrapper that works with `MonadJournal`.
///
/// Custom replacements for the staking and reserve-balance addresses must use identifiers other
/// than `MonadStaking` and `MonadReserveBalance`. Those identifiers are reserved for native
/// entries because they select handlers that require the full Monad execution context.
/// The native reserve-balance entry cannot be moved to another address because its protocol
/// behavior also requires that context; replace it with a custom precompile before moving it.
#[derive(Debug)]
pub struct MonadPrecompilesMap {
    inner: PrecompilesMap,
    spec: MonadHardfork,
    runtime_spec: Arc<AtomicU8>,
    reserve_balance_management: ReserveBalanceManagement,
    modified: bool,
}

impl MonadPrecompilesMap {
    /// Create a new Monad precompile map for the given spec.
    pub fn new_with_spec(spec: MonadHardfork) -> Self {
        let runtime_spec = Arc::new(AtomicU8::new(spec as u8));
        let mut inner = runtime_monad_precompiles(Arc::clone(&runtime_spec));
        extend_monad_precompiles_for_spec(&mut inner, spec);
        Self {
            inner,
            spec,
            runtime_spec,
            reserve_balance_management: ReserveBalanceManagement::Managed,
            modified: false,
        }
    }

    /// Returns the precompile addresses, including Monad-only precompiles.
    pub fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        self.inner.addresses().copied()
    }

    /// Returns whether the address is a Monad precompile.
    pub fn contains(&self, address: &Address) -> bool {
        self.inner.get(address).is_some()
    }

    fn contains_native_precompile(&self, address: &Address, id: &str) -> bool {
        self.inner
            .get(address)
            .is_some_and(|precompile| is_native_monad_id(precompile.precompile_id(), id))
    }

    fn inner_warm_addresses(&self) -> &AddressSet {
        <PrecompilesMap as PrecompileProvider<Context>>::warm_addresses(&self.inner)
    }

    fn update_reserve_balance_precompile(&mut self) {
        if self.reserve_balance_management != ReserveBalanceManagement::Managed {
            return;
        }

        if MonadHardfork::MonadNine.is_enabled_in(self.spec) {
            self.inner
                .apply_precompile(&RESERVE_BALANCE_ADDRESS, |precompile| {
                    precompile.or_else(|| Some(monad_reserve_balance_precompile()))
                });
        } else {
            self.inner
                .apply_precompile(&RESERVE_BALANCE_ADDRESS, |precompile| {
                    precompile.filter(|precompile| {
                        !is_native_monad_id(precompile.precompile_id(), MONAD_RESERVE_BALANCE_ID)
                    })
                });
        }
    }

    fn reconcile_reserve_balance_management(&mut self) {
        self.reserve_balance_management = match self.inner.get(&RESERVE_BALANCE_ADDRESS) {
            Some(precompile)
                if is_native_monad_id(precompile.precompile_id(), MONAD_RESERVE_BALANCE_ID) =>
            {
                ReserveBalanceManagement::Managed
            }
            Some(_) => ReserveBalanceManagement::Overridden,
            None if MonadHardfork::MonadNine.is_enabled_in(self.spec) => {
                ReserveBalanceManagement::Removed
            }
            None if self.reserve_balance_management == ReserveBalanceManagement::Overridden => {
                ReserveBalanceManagement::Managed
            }
            None => self.reserve_balance_management,
        };
    }

    fn run_dynamic<DB: Database>(
        &mut self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let Some(precompile) = self.inner.get(&inputs.bytecode_address) else {
            return Ok(None);
        };

        let (block, tx, cfg, journaled_state, _, local) = context.all_mut();

        let precompile_result = precompile.call(PrecompileInput {
            data: inputs.input.as_bytes_local(local).as_ref(),
            gas: inputs.gas_limit,
            reservoir: inputs.reservoir,
            caller: inputs.caller,
            value: inputs.call_value(),
            is_static: inputs.is_static,
            internals: EvmInternals::new(journaled_state, block, cfg, tx),
            target_address: inputs.target_address,
            bytecode_address: inputs.bytecode_address,
        });

        let output = precompile_result.map_err(|e| e.to_string())?;
        if let Some(halt_reason) = output.halt_reason() {
            if !halt_reason.is_oog() && context.journal().depth() == 1 {
                context
                    .local_mut()
                    .set_precompile_error_context(halt_reason.to_string());
            }
        }

        Ok(Some(precompile_output_to_interpreter_result(
            output,
            inputs.gas_limit,
        )))
    }
}

impl Deref for MonadPrecompilesMap {
    type Target = PrecompilesMap;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MonadPrecompilesMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.modified = true;
        &mut self.inner
    }
}

impl<DB: Database> PrecompileProvider<MonadContext<DB>> for MonadPrecompilesMap {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: MonadHardfork) -> bool {
        let spec_changed = spec != self.spec;
        let modified = core::mem::take(&mut self.modified);
        if modified {
            self.reconcile_reserve_balance_management();
        }
        if spec_changed {
            self.spec = spec;
            self.runtime_spec.store(spec as u8, Ordering::Relaxed);
            self.update_reserve_balance_precompile();
        }
        spec_changed || modified
    }

    fn run(
        &mut self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if inputs.bytecode_address == STAKING_ADDRESS
            && self.contains_native_precompile(&inputs.bytecode_address, MONAD_STAKING_ID)
        {
            if let Some(result) = staking::run_staking_precompile(context, inputs)? {
                return Ok(Some(result));
            }
        }

        if inputs.bytecode_address == RESERVE_BALANCE_ADDRESS
            && self.contains_native_precompile(&inputs.bytecode_address, MONAD_RESERVE_BALANCE_ID)
        {
            if let Some(result) = reserve_balance::run_reserve_balance_precompile(context, inputs)?
            {
                return Ok(Some(result));
            }
        }

        self.run_dynamic(context, inputs)
    }

    fn warm_addresses(&self) -> &AddressSet {
        self.inner_warm_addresses()
    }

    fn contains(&self, address: &Address) -> bool {
        Self::contains(self, address)
    }
}

/// Monad EVM implementation.
///
/// This is a wrapper type around the `monad_revm::MonadEvm` with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`InnerMonadEvm`](monad_revm::MonadEvm) type.
#[allow(missing_debug_implementations)] // MonadEvm doesn't impl Debug
pub struct MonadEvm<DB: Database, I, P = MonadPrecompilesMap> {
    inner: InnerMonadEvm<MonadContext<DB>, I, MonadInstructions<MonadContext<DB>>, P>,
    inspect: bool,
}

impl<DB: Database, I, P> MonadEvm<DB, I, P> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &MonadContext<DB> {
        &self.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut MonadContext<DB> {
        &mut self.inner.0.ctx
    }
}

impl<DB: Database, I, P> MonadEvm<DB, I, P> {
    /// Creates a new Monad EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`InnerMonadEvm`](monad_revm::MonadEvm) should be invoked on [`Evm::transact`].
    pub const fn new(
        evm: InnerMonadEvm<MonadContext<DB>, I, MonadInstructions<MonadContext<DB>>, P>,
        inspect: bool,
    ) -> Self {
        Self {
            inner: evm,
            inspect,
        }
    }
}

impl<DB: Database, I, P> Deref for MonadEvm<DB, I, P> {
    type Target = MonadContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for MonadEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> Evm for MonadEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<MonadContext<DB>>,
    P: PrecompileProvider<MonadContext<DB>, Output = InterpreterResult>
        + Deref<Target = PrecompilesMap>
        + DerefMut,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = MonadHardfork;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        self.cfg.inner()
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_tx(tx)
        } else {
            self.inner.transact(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner
                .inspect_system_call_with_caller(caller, contract, data)
        } else {
            self.inner.system_call_with_caller(caller, contract, data)
        }
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let Context {
            block: block_env,
            cfg: monad_cfg,
            journaled_state,
            ..
        } = self.inner.0.ctx;
        // Convert MonadCfgEnv back to CfgEnv<MonadHardfork> for EvmEnv
        let cfg_env = monad_cfg.into_inner();

        (
            journaled_state.into_database(),
            EvmEnv { block_env, cfg_env },
        )
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.0.ctx.journaled_state.database,
            &self.inner.0.inspector,
            &self.inner.0.precompiles,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.0.ctx.journaled_state.database,
            &mut self.inner.0.inspector,
            &mut self.inner.0.precompiles,
        )
    }
}

/// Factory for creating [`MonadEvm`] instances.
///
/// Implements [`alloy_evm::EvmFactory`] for integration with Foundry.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MonadEvmFactory;

impl EvmFactory for MonadEvmFactory {
    type Evm<DB: Database, I: Inspector<MonadContext<DB>>> = MonadEvm<DB, I>;
    type Context<DB: Database> = MonadContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = MonadHardfork;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<MonadHardfork>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        // Convert CfgEnv<MonadHardfork> to MonadCfgEnv for Monad-specific defaults (128KB code size)
        let monad_cfg = MonadCfgEnv::from(input.cfg_env);

        MonadEvm {
            inner: monad_context_with_db(db)
                .with_block(input.block_env)
                .with_cfg(monad_cfg)
                .build_monad_with_inspector(NoOpInspector {})
                .with_precompiles(MonadPrecompilesMap::new_with_spec(spec_id)),
            inspect: false,
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<MonadHardfork>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        // Convert CfgEnv<MonadHardfork> to MonadCfgEnv for Monad-specific defaults (128KB code size)
        let monad_cfg = MonadCfgEnv::from(input.cfg_env);

        MonadEvm {
            inner: monad_context_with_db(db)
                .with_block(input.block_env)
                .with_cfg(monad_cfg)
                .build_monad_with_inspector(inspector)
                .with_precompiles(MonadPrecompilesMap::new_with_spec(spec_id)),
            inspect: true,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PrecompilesMap Integration
// ═══════════════════════════════════════════════════════════════════════════════

/// Extend a `PrecompilesMap` with Monad precompiles for a specific Monad spec.
///
/// This explicitly registers Monad-only addresses in the exposed
/// `PrecompilesMap` so Foundry diagnostics and warm-address logic see the same
/// precompile set that `MonadPrecompilesMap` dispatches internally.
/// Existing custom entries at Monad protocol addresses are preserved.
///
/// # Example
///
/// ```ignore
/// use alloy_evm::precompiles::PrecompilesMap;
/// use alloy_monad_evm::extend_monad_precompiles_for_spec;
/// use monad_revm::MonadHardfork;
///
/// let mut precompiles = PrecompilesMap::default();
/// extend_monad_precompiles_for_spec(&mut precompiles, MonadHardfork::MonadNine);
/// ```
pub fn extend_monad_precompiles_for_spec(precompiles: &mut PrecompilesMap, spec: MonadHardfork) {
    extend_monad_staking_precompile(precompiles);

    if MonadHardfork::MonadNine.is_enabled_in(spec) {
        extend_monad_reserve_balance_precompile(precompiles);
    } else {
        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |precompile| {
            precompile.filter(|precompile| {
                !is_native_monad_id(precompile.precompile_id(), MONAD_RESERVE_BALANCE_ID)
            })
        });
    }
}

fn extend_monad_staking_precompile(precompiles: &mut PrecompilesMap) {
    if precompiles.get(&STAKING_ADDRESS).is_some() {
        return;
    }
    precompiles.apply_precompile(&STAKING_ADDRESS, |_| {
        Some(DynPrecompile::new_stateful(
            PrecompileId::Custom(MONAD_STAKING_ID.into()),
            |input: PrecompileInput<'_>| -> Result<PrecompileOutput, PrecompileError> {
                // Reject DELEGATECALL/CALLCODE (target_address != bytecode_address)
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
                }

                // Reject STATICCALL and calls inside a static frame
                if input.is_static {
                    return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
                }

                // Decode selector — short input routes to fallback via write path
                let selector: [u8; 4] = match input.data.get(..4).and_then(|s| s.try_into().ok()) {
                    Some(s) => s,
                    None => {
                        // Route short input through write path for proper fallback handling
                        let mut storage = PrecompileInputStakingStorage {
                            internals: input.internals,
                        };
                        let result = staking::write::run_staking_write(
                            input.data,
                            input.gas,
                            &mut storage,
                            &input.caller,
                            input.value,
                        )
                        .map_err(PrecompileError::Fatal)?;
                        return interpreter_result_to_output(input.reservoir, result);
                    }
                };

                // Route write selectors through the write module (payability checked per-method inside)
                if staking::write::is_write_selector(selector) {
                    let mut storage = PrecompileInputStakingStorage {
                        internals: input.internals,
                    };
                    let caller = input.caller;
                    let call_value = input.value;
                    match staking::write::run_staking_write(
                        input.data,
                        input.gas,
                        &mut storage,
                        &caller,
                        call_value,
                    ) {
                        Ok(result) => interpreter_result_to_output(input.reservoir, result),
                        Err(e) => Err(PrecompileError::Fatal(e)),
                    }
                } else {
                    // Read operations (payability checked per-method inside)
                    let mut reader = PrecompileInputStakingStorage {
                        internals: input.internals,
                    };
                    match staking::run_staking_with_reader(
                        input.data,
                        input.gas,
                        &mut reader,
                        input.value,
                    ) {
                        Ok(result) => interpreter_result_to_output(input.reservoir, result),
                        Err(e) => Err(PrecompileError::Fatal(e)),
                    }
                }
            },
        ))
    });
}

fn extend_monad_reserve_balance_precompile(precompiles: &mut PrecompilesMap) {
    if precompiles.get(&RESERVE_BALANCE_ADDRESS).is_some() {
        return;
    }
    precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| {
        Some(monad_reserve_balance_precompile())
    });
}

fn monad_reserve_balance_precompile() -> DynPrecompile {
    DynPrecompile::new_stateful(
        PrecompileId::Custom(MONAD_RESERVE_BALANCE_ID.into()),
        |input: PrecompileInput<'_>| -> Result<PrecompileOutput, PrecompileError> {
            // Runtime dispatch for this address is handled before `run_dynamic`;
            // this entry keeps the exposed `PrecompilesMap` metadata complete.
            Ok(PrecompileOutput::halt(
                PrecompileHalt::other_static(
                    "reserve-balance execution requires MonadPrecompilesMap",
                ),
                input.reservoir,
            ))
        },
    )
}

/// Convert an `InterpreterResult` to a `PrecompileOutput`.
fn interpreter_result_to_output(
    reservoir: u64,
    result: InterpreterResult,
) -> Result<PrecompileOutput, PrecompileError> {
    let gas_used = result.gas.total_gas_spent();
    if result.result == InstructionResult::Return {
        Ok(PrecompileOutput::new(gas_used, result.output, reservoir))
    } else if result.result == InstructionResult::PrecompileOOG {
        Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))
    } else {
        // Revert
        Ok(PrecompileOutput::revert(gas_used, result.output, reservoir))
    }
}

/// Storage implementation that uses `PrecompileInput.internals` for both reads and writes.
struct PrecompileInputStakingStorage<'a> {
    internals: alloy_evm::EvmInternals<'a>,
}

impl StorageReader for PrecompileInputStakingStorage<'_> {
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileHalt> {
        self.internals
            .sload(STAKING_ADDRESS, key)
            .map(|r| r.data)
            .map_err(|e| PrecompileHalt::other(format!("Storage read failed: {e:?}")))
    }
}

impl StakingStorage for PrecompileInputStakingStorage<'_> {
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileHalt> {
        self.internals
            .sstore(STAKING_ADDRESS, key, value)
            .map(|_| ())
            .map_err(|e| PrecompileHalt::other(format!("Storage write failed: {e:?}")))
    }

    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<(), PrecompileHalt> {
        if amount.is_zero() {
            return Ok(());
        }
        match self.internals.transfer(from, to, amount) {
            Ok(None) => Ok(()),
            Ok(Some(e)) => Err(PrecompileHalt::other(format!("Transfer failed: {e:?}"))),
            Err(e) => Err(PrecompileHalt::other(format!("Transfer error: {e:?}"))),
        }
    }

    fn emit_log(&mut self, log: revm::primitives::Log) -> Result<(), PrecompileHalt> {
        self.internals.log(log);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monad_revm::{api::block::syscall_snapshot_calldata, staking::constants::SYSTEM_ADDRESS};
    use revm::{
        bytecode::opcode,
        database::{EmptyDB, InMemoryDB},
        inspector::CountInspector,
        interpreter::{CallInput, CallScheme, CallValue},
        precompile::u64_to_address,
        primitives::B256,
        state::{AccountInfo, Bytecode},
    };

    fn assert_precompiles_map_factory<F: EvmFactory<Precompiles = PrecompilesMap>>() {}

    fn set_spec(precompiles: &mut MonadPrecompilesMap, spec: MonadHardfork) -> bool {
        PrecompileProvider::<MonadContext<EmptyDB>>::set_spec(precompiles, spec)
    }

    fn call_inputs(address: Address, input: Bytes) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(input),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            reservoir: 0,
            bytecode_address: address,
            known_bytecode: (B256::ZERO, Bytecode::default()),
            target_address: address,
            caller: Address::ZERO,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        }
    }

    fn run_precompile(
        precompiles: &mut MonadPrecompilesMap,
        spec: MonadHardfork,
        address: Address,
        input: Bytes,
    ) -> Option<InterpreterResult> {
        let mut context =
            monad_context_with_db(EmptyDB::default()).with_cfg(MonadCfgEnv::new_with_spec(spec));
        PrecompileProvider::<MonadContext<EmptyDB>>::run(
            precompiles,
            &mut context,
            &call_inputs(address, input),
        )
        .expect("precompile execution should not fail fatally")
    }

    fn custom_precompile(id: &'static str, gas_used: u64, output: &'static [u8]) -> DynPrecompile {
        let output = Bytes::from_static(output);
        DynPrecompile::new(PrecompileId::Custom(id.into()), move |input| {
            Ok(PrecompileOutput::new(
                gas_used,
                output.clone(),
                input.reservoir,
            ))
        })
    }

    fn assert_custom_result(result: InterpreterResult, gas_used: u64, output: &'static [u8]) {
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.total_gas_spent(), gas_used);
        assert_eq!(result.output, Bytes::from_static(output));
    }

    fn assert_precompile_absent(
        precompiles: &mut MonadPrecompilesMap,
        spec: MonadHardfork,
        address: Address,
    ) {
        assert!(!MonadPrecompilesMap::contains(precompiles, &address));
        assert!(!precompiles
            .addresses()
            .any(|candidate| candidate == address));
        assert!(!precompiles.inner_warm_addresses().contains(&address));
        assert!(run_precompile(precompiles, spec, address, Bytes::new()).is_none());
    }

    #[test]
    fn monad_factory_exposes_precompiles_map() {
        assert_precompiles_map_factory::<MonadEvmFactory>();
    }

    #[test]
    fn system_call_inspection_matches_uninspected_execution() {
        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            contract,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(vec![
                opcode::PUSH1,
                0x01,
                opcode::PUSH1,
                0x00,
                opcode::SSTORE,
                opcode::STOP,
            ]))),
        );
        let env = EvmEnv::new(
            CfgEnv::new_with_spec(MonadHardfork::MonadNine),
            BlockEnv::default(),
        );

        let mut uninspected = MonadEvmFactory.create_evm(db.clone(), env.clone());
        let expected = uninspected
            .transact_system_call(caller, contract, Bytes::new())
            .expect("uninspected system call should succeed");

        let mut inspected =
            MonadEvmFactory.create_evm_with_inspector(db, env, CountInspector::default());
        let actual = inspected
            .transact_system_call(caller, contract, Bytes::new())
            .expect("inspected system call should succeed");

        assert_eq!(actual, expected);
        assert!(actual.result.is_success());
        assert!(!actual.state.is_empty());
        assert!(inspected.components().1.call_count() > 0);
        assert!(inspected.components().1.step_count() > 0);
    }

    #[test]
    fn staking_system_call_invokes_inspector() {
        let mut evm = MonadEvmFactory.create_evm_with_inspector(
            revm::database::EmptyDB::default(),
            EvmEnv::new(
                CfgEnv::new_with_spec(MonadHardfork::MonadNine),
                BlockEnv::default(),
            ),
            CountInspector::default(),
        );

        let result = evm
            .transact_system_call(SYSTEM_ADDRESS, STAKING_ADDRESS, syscall_snapshot_calldata())
            .expect("staking system call should succeed");

        assert!(result.result.is_success());
        assert!(evm.components().1.call_count() > 0);
    }

    fn factory_exposes_precompile(spec: MonadHardfork, address: Address) -> bool {
        let evm = MonadEvmFactory.create_evm(
            revm::database::EmptyDB::default(),
            EvmEnv::new(CfgEnv::new_with_spec(spec), BlockEnv::default()),
        );

        let contains = evm
            .precompiles()
            .addresses()
            .any(|precompile_address| *precompile_address == address);

        contains
    }

    #[test]
    fn monad_factory_exposes_staking_precompile_address() {
        for spec in [
            MonadHardfork::MonadEight,
            MonadHardfork::MonadNine,
            MonadHardfork::MonadNext,
        ] {
            assert!(factory_exposes_precompile(spec, STAKING_ADDRESS));
        }
    }

    #[test]
    fn monad_factory_exposes_reserve_balance_precompile_address_when_enabled() {
        assert!(!factory_exposes_precompile(
            MonadHardfork::MonadEight,
            RESERVE_BALANCE_ADDRESS
        ));
        assert!(factory_exposes_precompile(
            MonadHardfork::MonadNine,
            RESERVE_BALANCE_ADDRESS
        ));
        assert!(factory_exposes_precompile(
            MonadHardfork::MonadNext,
            RESERVE_BALANCE_ADDRESS
        ));
    }

    #[test]
    fn staking_precompile_is_available_on_all_monad_specs() {
        for spec in [
            MonadHardfork::MonadEight,
            MonadHardfork::MonadNine,
            MonadHardfork::MonadNext,
        ] {
            let precompiles = MonadPrecompilesMap::new_with_spec(spec);
            let addresses = precompiles.addresses().collect::<Vec<_>>();

            assert!(precompiles.contains(&STAKING_ADDRESS));
            assert!(addresses.contains(&STAKING_ADDRESS));
        }
    }

    #[test]
    fn reserve_balance_precompile_is_gated_to_monad_nine_and_later() {
        let monad_eight = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);
        let monad_nine = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadNine);
        let monad_next = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadNext);

        assert!(!monad_eight.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(!monad_eight
            .addresses()
            .any(|address| address == RESERVE_BALANCE_ADDRESS));

        assert!(monad_nine.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(monad_nine
            .addresses()
            .any(|address| address == RESERVE_BALANCE_ADDRESS));

        assert!(monad_next.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(monad_next
            .addresses()
            .any(|address| address == RESERVE_BALANCE_ADDRESS));
    }

    #[test]
    fn reserve_balance_metadata_precompile_halts_without_fatal_error() {
        let monad_precompiles = MonadPrecompiles::new_with_spec(MonadHardfork::MonadNine);
        let mut precompiles = PrecompilesMap::from_static(monad_precompiles.precompiles());
        extend_monad_precompiles_for_spec(&mut precompiles, MonadHardfork::MonadNine);

        let reserve_balance = precompiles
            .get(&RESERVE_BALANCE_ADDRESS)
            .expect("reserve-balance precompile should be exposed in MonadNine");
        let mut context = monad_context_with_db(revm::database::EmptyDB::default());

        let output = reserve_balance
            .call(PrecompileInput {
                data: &[],
                gas: 100_000,
                reservoir: 7,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: RESERVE_BALANCE_ADDRESS,
                is_static: false,
                bytecode_address: RESERVE_BALANCE_ADDRESS,
                internals: EvmInternals::from_context(&mut context),
            })
            .expect("metadata precompile should halt without a fatal error");

        let halt_reason = output
            .halt_reason()
            .expect("metadata precompile should halt");
        assert!(!halt_reason.is_oog());
        assert_eq!(
            halt_reason.to_string(),
            "reserve-balance execution requires MonadPrecompilesMap"
        );
        assert_eq!(output.reservoir, 7);
    }

    fn modexp_input(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(96 + base.len() + exponent.len() + modulus.len());
        input.extend_from_slice(&U256::from(base.len()).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(exponent.len()).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(modulus.len()).to_be_bytes::<32>());
        input.extend_from_slice(base);
        input.extend_from_slice(exponent);
        input.extend_from_slice(modulus);
        input
    }

    fn execute_modexp(precompiles: &MonadPrecompilesMap, input: &[u8]) -> PrecompileOutput {
        let precompile = precompiles
            .inner
            .get(&u64_to_address(5))
            .expect("MODEXP precompile should be present");
        let mut context = monad_context_with_db(revm::database::EmptyDB::default());
        precompile
            .call(PrecompileInput {
                data: input,
                gas: 10_000_000,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: u64_to_address(5),
                is_static: false,
                bytecode_address: u64_to_address(5),
                internals: EvmInternals::from_context(&mut context),
            })
            .expect("MODEXP execution should succeed")
    }

    #[test]
    fn set_spec_updates_monad_only_precompile_set() {
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);

        assert!(!precompiles.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        assert!(precompiles.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(precompiles
            .inner_warm_addresses()
            .contains(&RESERVE_BALANCE_ADDRESS));
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));
        assert!(!precompiles.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(!precompiles
            .inner_warm_addresses()
            .contains(&RESERVE_BALANCE_ADDRESS));
        assert!(!set_spec(&mut precompiles, MonadHardfork::MonadEight));
    }

    #[test]
    fn runtime_selected_protocol_precompiles_do_not_support_caching() {
        let runtime_spec = Arc::new(AtomicU8::new(MonadHardfork::MonadEight as u8));
        let precompiles = runtime_monad_precompiles(runtime_spec);

        for address in static_monad_precompiles(MonadHardfork::MonadEight).addresses() {
            let precompile = precompiles
                .get(address)
                .expect("runtime-selected protocol precompile should be present");
            assert!(
                !precompile.supports_caching(),
                "runtime-selected precompile at {address} must not be cached"
            );
        }
    }

    #[test]
    fn set_spec_selects_modexp_pricing_in_both_directions() {
        let input = modexp_input(&[0xff; 32], &[0xff; 32], &[0xff; 32]);
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);

        let monad_eight_gas = execute_modexp(&precompiles, &input).gas_used;
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        let monad_nine_gas = execute_modexp(&precompiles, &input).gas_used;
        assert!(
            monad_nine_gas > monad_eight_gas,
            "MonadNine MODEXP gas should exceed MonadEight"
        );

        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));
        assert_eq!(
            execute_modexp(&precompiles, &input).gas_used,
            monad_eight_gas
        );
    }

    #[test]
    fn set_spec_preserves_injected_precompiles_and_overrides() {
        let custom_address = Address::from([0x44; 20]);
        let modexp_address = u64_to_address(5);
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);
        precompiles.apply_precompile(&custom_address, |_| {
            Some(custom_precompile("InjectedPrecompile", 17, b"custom"))
        });
        precompiles.apply_precompile(&modexp_address, |_| {
            Some(custom_precompile("ModexpOverride", 23, b"modexp"))
        });
        precompiles.apply_precompile(&STAKING_ADDRESS, |_| {
            Some(custom_precompile("StakingOverride", 27, b"staking"))
        });
        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| {
            Some(custom_precompile("ReserveBalanceOverride", 29, b"reserve"))
        });

        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        assert!(precompiles.inner.get(&custom_address).is_some());
        assert!(precompiles.inner_warm_addresses().contains(&custom_address));
        assert_eq!(execute_modexp(&precompiles, &[]).gas_used, 23);
        assert_custom_result(
            run_precompile(
                &mut precompiles,
                MonadHardfork::MonadNine,
                STAKING_ADDRESS,
                Bytes::new(),
            )
            .expect("staking override should execute"),
            27,
            b"staking",
        );
        assert_custom_result(
            run_precompile(
                &mut precompiles,
                MonadHardfork::MonadNine,
                RESERVE_BALANCE_ADDRESS,
                Bytes::new(),
            )
            .expect("reserve-balance override should execute"),
            29,
            b"reserve",
        );

        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));
        assert!(precompiles.inner.get(&custom_address).is_some());
        assert_eq!(execute_modexp(&precompiles, &[]).gas_used, 23);
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        assert_custom_result(
            run_precompile(
                &mut precompiles,
                MonadHardfork::MonadNine,
                RESERVE_BALANCE_ADDRESS,
                Bytes::new(),
            )
            .expect("reserve-balance override should survive the round trip"),
            29,
            b"reserve",
        );
    }

    #[test]
    fn removed_monad_precompiles_stay_removed_across_transitions() {
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadNine);
        precompiles.apply_precompile(&STAKING_ADDRESS, |_| None);
        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| None);

        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        assert_precompile_absent(&mut precompiles, MonadHardfork::MonadNine, STAKING_ADDRESS);
        assert_precompile_absent(
            &mut precompiles,
            MonadHardfork::MonadNine,
            RESERVE_BALANCE_ADDRESS,
        );

        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));
        assert_precompile_absent(&mut precompiles, MonadHardfork::MonadNine, STAKING_ADDRESS);
        assert_precompile_absent(
            &mut precompiles,
            MonadHardfork::MonadNine,
            RESERVE_BALANCE_ADDRESS,
        );
    }

    #[test]
    fn removing_monad_eight_override_restores_native_reserve_balance() {
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);
        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| {
            Some(custom_precompile("ReserveBalanceOverride", 29, b"reserve"))
        });
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));

        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| None);
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadNine));

        assert_eq!(
            precompiles
                .inner
                .get(&RESERVE_BALANCE_ADDRESS)
                .expect("native reserve-balance precompile should be restored")
                .precompile_id()
                .name(),
            MONAD_RESERVE_BALANCE_ID
        );
        let result = run_precompile(
            &mut precompiles,
            MonadHardfork::MonadNine,
            RESERVE_BALANCE_ADDRESS,
            Bytes::copy_from_slice(&monad_revm::reserve_balance::abi::DIPPED_INTO_RESERVE_SELECTOR),
        )
        .expect("native reserve-balance precompile should execute");
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(
            result.gas.total_gas_spent(),
            monad_revm::reserve_balance::abi::DIPPED_INTO_RESERVE_GAS
        );
        assert_eq!(result.output, Bytes::from([0u8; 32]));
    }

    #[test]
    fn same_spec_mutations_refresh_warm_addresses_through_evm_api() {
        let custom_address = Address::from([0x55; 20]);
        let mut evm = MonadEvmFactory.create_evm(
            EmptyDB::default(),
            EvmEnv::new(
                CfgEnv::new_with_spec(MonadHardfork::MonadEight),
                BlockEnv::default(),
            ),
        );

        evm.precompiles_mut()
            .apply_precompile(&custom_address, |_| {
                Some(custom_precompile("InjectedPrecompile", 17, b"custom"))
            });
        assert!(set_spec(
            &mut evm.inner.0.precompiles,
            MonadHardfork::MonadEight
        ));
        assert!(evm
            .inner
            .0
            .precompiles
            .inner_warm_addresses()
            .contains(&custom_address));

        evm.precompiles_mut()
            .apply_precompile(&custom_address, |_| None);
        assert!(set_spec(
            &mut evm.inner.0.precompiles,
            MonadHardfork::MonadEight
        ));
        assert!(!evm
            .inner
            .0
            .precompiles
            .inner_warm_addresses()
            .contains(&custom_address));
        assert!(!set_spec(
            &mut evm.inner.0.precompiles,
            MonadHardfork::MonadEight
        ));
        assert!(set_spec(
            &mut evm.inner.0.precompiles,
            MonadHardfork::MonadNine
        ));
        assert!(evm.inner.0.precompiles.contains(&RESERVE_BALANCE_ADDRESS));
    }

    #[test]
    fn extending_precompiles_preserves_custom_monad_entries() {
        let static_precompiles = MonadPrecompiles::new_with_spec(MonadHardfork::MonadEight);
        let mut precompiles = PrecompilesMap::from_static(static_precompiles.precompiles());
        precompiles.apply_precompile(&STAKING_ADDRESS, |_| {
            Some(custom_precompile("StakingOverride", 27, b"staking"))
        });
        precompiles.apply_precompile(&RESERVE_BALANCE_ADDRESS, |_| {
            Some(custom_precompile("ReserveBalanceOverride", 29, b"reserve"))
        });

        extend_monad_precompiles_for_spec(&mut precompiles, MonadHardfork::MonadEight);
        extend_monad_precompiles_for_spec(&mut precompiles, MonadHardfork::MonadNine);

        assert_eq!(
            precompiles
                .get(&STAKING_ADDRESS)
                .expect("staking override should remain installed")
                .precompile_id()
                .name(),
            "StakingOverride"
        );
        assert_eq!(
            precompiles
                .get(&RESERVE_BALANCE_ADDRESS)
                .expect("reserve-balance override should remain installed")
                .precompile_id()
                .name(),
            "ReserveBalanceOverride"
        );
    }

    #[test]
    fn moved_staking_precompile_executes_from_the_map() {
        let moved_address = Address::from([0x66; 20]);
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadHardfork::MonadEight);
        precompiles
            .move_precompiles([(STAKING_ADDRESS, moved_address)])
            .expect("native staking precompile should be movable");
        assert!(set_spec(&mut precompiles, MonadHardfork::MonadEight));

        assert_precompile_absent(&mut precompiles, MonadHardfork::MonadEight, STAKING_ADDRESS);
        let result = run_precompile(
            &mut precompiles,
            MonadHardfork::MonadEight,
            moved_address,
            Bytes::new(),
        )
        .expect("moved staking precompile should execute dynamically");
        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(result.output, Bytes::from_static(b"method not supported"));
    }
}
