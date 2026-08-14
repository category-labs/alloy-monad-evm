# Alloy Monad EVM

[![Crates.io](https://img.shields.io/crates/v/alloy-monad-evm.svg)](https://crates.io/crates/alloy-monad-evm)
[![Documentation](https://docs.rs/alloy-monad-evm/badge.svg)](https://docs.rs/alloy-monad-evm)
[![License](https://img.shields.io/crates/l/alloy-monad-evm.svg)](LICENSE)

`alloy-monad-evm` is the Alloy integration layer for Monad execution.

It wraps [`monad-revm`](https://crates.io/crates/monad-revm) behind `alloy-evm` traits so Foundry/Alloy-based execution stacks can instantiate Monad EVMs through standard interfaces.

For the staking precompile design and detailed semantics, see the `monad-revm` README:

- https://github.com/category-labs/monad-revm

## Compatibility

| Component | Version |
|-----------|---------|
| `alloy-evm` | `0.38.0` |
| `alloy-primitives` | `1.6.1` |
| `monad-revm` | `0.6.0` |
| `revm` | `42.0.1` |
| Rust MSRV | `1.94.1` |

## What this crate adds on top of `monad-revm`

1. `MonadEvm`: `alloy_evm::Evm` implementation wrapping `monad_revm::MonadEvm`.
2. `MonadEvmFactory`: `alloy_evm::EvmFactory` implementation for building Monad EVM instances from Alloy environments.
3. `extend_monad_precompiles_for_spec`: helper that registers Monad precompile metadata into a `PrecompilesMap` for a specific Monad spec.

## Staking integration at Alloy level

`alloy-monad-evm` does not reimplement staking logic. It delegates execution to `monad-revm` staking modules and focuses on wiring:

- Registers Monad-only precompile addresses via `PrecompilesMap::apply_precompile` so they are discoverable in precompile address sets.
- Ensures precompile-aware tooling behavior (for example, Foundry warm precompile handling and better revert diagnostics).
- Routes write selectors through `monad_revm::staking::write::run_staking_write`.
- Routes read selectors through `monad_revm::staking::run_staking_with_reader`.
- Enforces direct-call behavior (`DELEGATECALL`/`CALLCODE`, delegated top-level calls, and static contexts are rejected in this integration path).

This keeps staking behavior centralized in one place (`monad-revm`) while allowing Alloy-based runtimes to execute the same semantics.

## Monad-specific behavior exposed through this crate

- Monad gas model (cold access repricing, no refunds).
- Monad precompile repricing.
- Staking precompile at `0x1000` (read + write + syscalls, via `monad-revm`).
- Reserve-balance precompile execution and metadata at `0x1001` for MonadNine and later.
- Per-frame protocol precompile selection when execution crosses Monad hardforks.
- Mutable precompile overrides and removals that remain authoritative for dispatch and warming.

## Installation

```toml
[dependencies]
alloy-monad-evm = "0.6.0"
monad-revm = "0.6.0"
```

## Usage

### Factory-based usage

```rust
use alloy_evm::EvmFactory;
use alloy_monad_evm::MonadEvmFactory;

let factory = MonadEvmFactory::default();
let evm = factory.create_evm(db, env);
```

### Extending a `PrecompilesMap`

```rust
use alloy_evm::precompiles::PrecompilesMap;
use alloy_monad_evm::extend_monad_precompiles_for_spec;
use monad_revm::{precompiles::MonadPrecompiles, MonadHardfork};

let spec = MonadHardfork::MonadNine;
let monad_precompiles = MonadPrecompiles::new_with_spec(spec);
let mut precompiles = PrecompilesMap::from_static(monad_precompiles.precompiles());
extend_monad_precompiles_for_spec(&mut precompiles, spec);
```

## Crate surface

- `MonadEvm`
- `MonadEvmFactory`
- `MonadPrecompilesMap`
- `MonadContext` (re-export from `monad-revm`)
- `MonadHandler` (re-export from `monad-revm`)
- `extend_monad_precompiles_for_spec`

## Feature flags

- `std` (default): Enables standard-library support for `alloy-primitives`, `revm`, `alloy-evm`, and `monad-revm`.
- `memory_limit` (default): Enables Monad MIP-3 memory-limit support through `monad-revm`.
- `asm-keccak`: Enables platform-dependent assembly Keccak implementations in `alloy-evm`, `alloy-primitives`, and `revm`.

With default features disabled, the crate is `no_std` and uses `alloc`. CI checks both the base
configuration and `memory_limit` on `thumbv7em-none-eabi`; `asm-keccak` is not available on every
bare-metal target.

## License

Licensed under [MIT license](LICENSE).
