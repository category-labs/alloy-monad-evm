# Alloy Monad EVM

[![Crates.io](https://img.shields.io/crates/v/alloy-monad-evm.svg)](https://crates.io/crates/alloy-monad-evm)
[![Documentation](https://docs.rs/alloy-monad-evm/badge.svg)](https://docs.rs/alloy-monad-evm)
[![License](https://img.shields.io/crates/l/alloy-monad-evm.svg)](LICENSE)

`alloy-monad-evm` is the Alloy integration layer for Monad execution.

It wraps [`monad-revm`](https://crates.io/crates/monad-revm) behind `alloy-evm` traits so Foundry/Alloy-based execution stacks can instantiate Monad EVMs through standard interfaces.

For the staking precompile design and detailed semantics, see the `monad-revm` README:

- https://github.com/category-labs/monad-revm

## What this crate adds on top of `monad-revm`

1. `MonadEvm`: `alloy_evm::Evm` implementation wrapping `monad_revm::MonadEvm`.
2. `MonadEvmFactory`: `alloy_evm::EvmFactory` implementation for building Monad EVM instances from Alloy environments.
3. `extend_monad_precompiles`: helper that registers Monad staking precompile (`0x1000`) into a `PrecompilesMap`.

## Staking integration at Alloy level

`alloy-monad-evm` does not reimplement staking logic. It delegates execution to `monad-revm` staking modules and focuses on wiring:

- Registers `0x1000` via `PrecompilesMap::apply_precompile` so the address is discoverable in precompile address sets.
- Ensures precompile-aware tooling behavior (for example, Foundry warm precompile handling and better revert diagnostics).
- Routes write selectors through `monad_revm::staking::write::run_staking_write`.
- Routes read selectors through `monad_revm::staking::run_staking_with_reader`.
- Enforces direct-call behavior (`DELEGATECALL`/`CALLCODE` and static contexts are rejected in this integration path).

This keeps staking behavior centralized in one place (`monad-revm`) while allowing Alloy-based runtimes to execute the same semantics.

## Monad-specific behavior exposed through this crate

- Monad gas model (cold access repricing, no refunds).
- Monad precompile repricing.
- Staking precompile at `0x1000` (read + write + syscalls, via `monad-revm`).

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
use alloy_monad_evm::extend_monad_precompiles;

let mut precompiles = PrecompilesMap::default();
extend_monad_precompiles(&mut precompiles);
```

## Crate surface

- `MonadEvm`
- `MonadEvmFactory`
- `MonadContext` (re-export from `monad-revm`)
- `MonadHandler` (re-export from `monad-revm`)
- `extend_monad_precompiles`

## Feature flags

- `std` (default)
- `asm-keccak`

## License

Licensed under [MIT license](LICENSE).
