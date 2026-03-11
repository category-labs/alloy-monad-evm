#![cfg_attr(not(test), warn(unused_crate_dependencies))]

//! Alloy EVM implementation for Monad blockchain.
//!
//! This crate provides:
//! - [`MonadEvm`]: Wrapper implementing [`alloy_evm::Evm`] trait
//! - [`MonadEvmFactory`]: Factory implementing [`alloy_evm::EvmFactory`] trait
//! - [`MonadContext`]: Type alias for Monad EVM context (re-exported from monad-revm)
//! - [`extend_monad_precompiles`]: Function to extend `PrecompilesMap` with staking precompile

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
    MonadBuilder, MonadCfgEnv, MonadEvm as InnerMonadEvm, MonadSpecId,
};
use revm::{
    context::{BlockEnv, TxEnv},
    context_interface::result::{EVMError, HaltReason, ResultAndState},
    context_interface::{ContextTr, JournalTr, LocalContextTr},
    handler::PrecompileProvider,
    inspector::NoOpInspector,
    interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, PrecompileId, PrecompileOutput},
    Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
};
use std::ops::{Deref, DerefMut};

// Re-export monad-revm types for external users
pub use monad_revm::{handler::MonadHandler, MonadContext};

/// Monad-aware precompile wrapper that works with `MonadJournal`.
#[derive(Clone, Debug)]
pub struct MonadPrecompilesMap {
    inner: PrecompilesMap,
    spec: MonadSpecId,
}

impl MonadPrecompilesMap {
    /// Create a new Monad precompile map for the given spec.
    pub fn new_with_spec(spec: MonadSpecId) -> Self {
        let monad_precompiles = MonadPrecompiles::new_with_spec(spec);
        let mut inner = PrecompilesMap::from_static(monad_precompiles.precompiles());
        extend_monad_precompiles(&mut inner);
        Self { inner, spec }
    }

    /// Returns the precompile addresses, including Monad-only precompiles.
    pub fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        let reserve_balance_enabled = MonadSpecId::MonadNine.is_enabled_in(self.spec);
        std::iter::once(STAKING_ADDRESS)
            .chain(reserve_balance_enabled.then_some(RESERVE_BALANCE_ADDRESS))
            .chain(self.inner.addresses().copied().filter(move |address| {
                *address != STAKING_ADDRESS
                    && (!reserve_balance_enabled || *address != RESERVE_BALANCE_ADDRESS)
            }))
    }

    /// Returns whether the address is a Monad precompile.
    pub fn contains(&self, address: &Address) -> bool {
        *address == STAKING_ADDRESS
            || (MonadSpecId::MonadNine.is_enabled_in(self.spec)
                && *address == RESERVE_BALANCE_ADDRESS)
            || self.inner.get(address).is_some()
    }

    fn run_dynamic<DB: Database>(
        &mut self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let Some(precompile) = self.inner.get(&inputs.bytecode_address) else {
            return Ok(None);
        };

        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };

        let (block, tx, cfg, journaled_state, _, local) = context.all_mut();

        let input_bytes = match &inputs.input {
            CallInput::SharedBuffer(range) => {
                if let Some(slice) = local.shared_memory_buffer_slice(range.clone()) {
                    slice.to_vec()
                } else {
                    Vec::new()
                }
            }
            CallInput::Bytes(bytes) => bytes.to_vec(),
        };

        let precompile_result = precompile.call(PrecompileInput {
            data: &input_bytes,
            gas: inputs.gas_limit,
            caller: inputs.caller,
            value: inputs.call_value(),
            is_static: inputs.is_static,
            internals: EvmInternals::new(journaled_state, block, cfg, tx),
            target_address: inputs.target_address,
            bytecode_address: inputs.bytecode_address,
        });

        match precompile_result {
            Ok(output) => {
                let underflow = result.gas.record_cost(output.gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = if output.reverted {
                    InstructionResult::Revert
                } else {
                    InstructionResult::Return
                };
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(error)) => return Err(error),
            Err(error) => {
                result.result = if error.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
                if !error.is_oog() && context.journal().depth() == 1 {
                    context
                        .local_mut()
                        .set_precompile_error_context(error.to_string());
                }
            }
        }

        Ok(Some(result))
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
        &mut self.inner
    }
}

impl<DB: Database> PrecompileProvider<MonadContext<DB>> for MonadPrecompilesMap {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: MonadSpecId) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec);
        true
    }

    fn run(
        &mut self,
        context: &mut MonadContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if let Some(result) = staking::run_staking_precompile(context, inputs)? {
            return Ok(Some(result));
        }

        if let Some(result) = reserve_balance::run_reserve_balance_precompile(context, inputs)? {
            return Ok(Some(result));
        }

        self.run_dynamic(context, inputs)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(self.addresses())
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
    P: PrecompileProvider<MonadContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = MonadSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
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
        self.inner.system_call_with_caller(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context {
            block: block_env,
            cfg: monad_cfg,
            journaled_state,
            ..
        } = self.inner.0.ctx;
        // Convert MonadCfgEnv back to CfgEnv<MonadSpecId> for EvmEnv
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
    type Evm<DB: Database, I: Inspector<MonadContext<DB>>> = MonadEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = MonadContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = MonadSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = MonadPrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<MonadSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        // Convert CfgEnv<MonadSpecId> to MonadCfgEnv for Monad-specific defaults (128KB code size)
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
        input: EvmEnv<MonadSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        // Convert CfgEnv<MonadSpecId> to MonadCfgEnv for Monad-specific defaults (128KB code size)
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

/// Extend a `PrecompilesMap` with Monad-specific precompiles.
///
/// This function adds the staking precompile (at address 0x1000) to the given
/// `PrecompilesMap` via `apply_precompile`, which explicitly registers the address
/// in the precompile address set. This ensures:
/// - 0x1000 appears in `addresses()` / `precompile_addresses()`
/// - Foundry's warm address set includes 0x1000
/// - Foundry's `RevertDiagnostic` inspector skips 0x1000 (no misleading
///   "call to non-contract address" on precompile reverts)
///
/// # Example
///
/// ```ignore
/// use alloy_evm::precompiles::PrecompilesMap;
/// use alloy_monad_evm::extend_monad_precompiles;
///
/// let mut precompiles = PrecompilesMap::default();
/// extend_monad_precompiles(&mut precompiles);
/// ```
pub fn extend_monad_precompiles(precompiles: &mut PrecompilesMap) {
    precompiles.apply_precompile(&STAKING_ADDRESS, |_| {
        Some(DynPrecompile::new_stateful(
            PrecompileId::Custom("MonadStaking".into()),
            |input: PrecompileInput<'_>| -> Result<PrecompileOutput, PrecompileError> {
                // Reject DELEGATECALL/CALLCODE (target_address != bytecode_address)
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
                }

                // Reject STATICCALL and calls inside a static frame
                if input.is_static {
                    return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
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
                        .map_err(|e| PrecompileError::Other(e.into()))?;
                        return interpreter_result_to_output(input.gas, result);
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
                        Ok(result) => interpreter_result_to_output(input.gas, result),
                        Err(e) => Err(PrecompileError::Other(e.into())),
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
                        Ok(result) => interpreter_result_to_output(input.gas, result),
                        Err(e) => Err(PrecompileError::Other(e.into())),
                    }
                }
            },
        ))
    });
}

/// Convert an `InterpreterResult` to a `PrecompileOutput`.
fn interpreter_result_to_output(
    gas_limit: u64,
    result: InterpreterResult,
) -> Result<PrecompileOutput, PrecompileError> {
    let gas_used = gas_limit.saturating_sub(result.gas.remaining());
    if result.result == InstructionResult::Return {
        Ok(PrecompileOutput::new(gas_used, result.output))
    } else if result.result == InstructionResult::PrecompileOOG {
        Err(PrecompileError::OutOfGas)
    } else {
        // Revert
        Ok(PrecompileOutput::new_reverted(gas_used, result.output))
    }
}

/// Storage implementation that uses `PrecompileInput.internals` for both reads and writes.
struct PrecompileInputStakingStorage<'a> {
    internals: alloy_evm::EvmInternals<'a>,
}

impl StorageReader for PrecompileInputStakingStorage<'_> {
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileError> {
        self.internals
            .sload(STAKING_ADDRESS, key)
            .map(|r| r.data)
            .map_err(|e| PrecompileError::Other(format!("Storage read failed: {e:?}").into()))
    }
}

impl StakingStorage for PrecompileInputStakingStorage<'_> {
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileError> {
        self.internals
            .sstore(STAKING_ADDRESS, key, value)
            .map(|_| ())
            .map_err(|e| PrecompileError::Other(format!("Storage write failed: {e:?}").into()))
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<(), PrecompileError> {
        if amount.is_zero() {
            return Ok(());
        }
        match self.internals.transfer(from, to, amount) {
            Ok(None) => Ok(()),
            Ok(Some(e)) => Err(PrecompileError::Other(
                format!("Transfer failed: {e:?}").into(),
            )),
            Err(e) => Err(PrecompileError::Other(
                format!("Transfer error: {e:?}").into(),
            )),
        }
    }

    fn emit_log(&mut self, log: revm::primitives::Log) -> Result<(), PrecompileError> {
        self.internals.log(log);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staking_precompile_is_available_on_all_monad_specs() {
        for spec in [
            MonadSpecId::MonadEight,
            MonadSpecId::MonadNine,
            MonadSpecId::MonadNext,
        ] {
            let precompiles = MonadPrecompilesMap::new_with_spec(spec);
            let addresses = precompiles.addresses().collect::<Vec<_>>();

            assert!(precompiles.contains(&STAKING_ADDRESS));
            assert!(addresses.contains(&STAKING_ADDRESS));
        }
    }

    #[test]
    fn reserve_balance_precompile_is_gated_to_monad_nine_and_later() {
        let monad_eight = MonadPrecompilesMap::new_with_spec(MonadSpecId::MonadEight);
        let monad_nine = MonadPrecompilesMap::new_with_spec(MonadSpecId::MonadNine);
        let monad_next = MonadPrecompilesMap::new_with_spec(MonadSpecId::MonadNext);

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
    fn set_spec_rebuilds_monad_only_precompile_set() {
        let mut precompiles = MonadPrecompilesMap::new_with_spec(MonadSpecId::MonadEight);

        assert!(!precompiles.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(
            PrecompileProvider::<MonadContext<revm::database::EmptyDB>>::set_spec(
                &mut precompiles,
                MonadSpecId::MonadNine
            )
        );
        assert!(precompiles.contains(&RESERVE_BALANCE_ADDRESS));
        assert!(
            !PrecompileProvider::<MonadContext<revm::database::EmptyDB>>::set_spec(
                &mut precompiles,
                MonadSpecId::MonadNine
            )
        );
    }
}
