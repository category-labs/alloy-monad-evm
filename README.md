# Alloy Monad EVM

[![Crates.io](https://img.shields.io/crates/v/alloy-monad-evm.svg)](https://crates.io/crates/alloy-monad-evm)
[![Documentation](https://docs.rs/alloy-monad-evm/badge.svg)](https://docs.rs/alloy-monad-evm)
[![License](https://img.shields.io/crates/l/alloy-monad-evm.svg)](LICENSE)

`alloy-monad-evm` provides Monad chain support by wrapping [monad-revm](https://crates.io/crates/monad-revm) with `alloy-evm` traits (`Evm`, `EvmFactory`).

## Features

- **MonadEvm**: Wrapper implementing `alloy_evm::Evm` trait
- **MonadEvmFactory**: Factory implementing `alloy_evm::EvmFactory` trait
- **MonadContext**: Type alias for Monad EVM context (re-exported from monad-revm)
- **extend_monad_precompiles**: Function to extend `PrecompilesMap` with staking precompile

## Monad-specific behavior

- Custom gas costs for cold storage/account access
- Custom precompile pricing
- Staking precompile at address `0x1000`

## Usage

```rust
use alloy_monad_evm::{MonadEvmFactory, extend_monad_precompiles};
use alloy_evm::EvmFactory;

// Create a Monad EVM using the factory
let factory = MonadEvmFactory::default();
let evm = factory.create_evm(db, env);
```

## License

Licensed under [MIT license](LICENSE).
