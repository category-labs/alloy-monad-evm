# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Fixed

- Report the full transaction gas limit in Alloy receipts and cumulative block gas for
  Monad transactions. Execution consumption remains available through `total_gas_spent()`.
- EIP-7702 delegation to Ethereum precompiles now returns empty output without charging precompile
  gas, including dynamic precompile overrides. Temporarily pinned `monad-revm` to the immutable
  fix commit pending its next release.
- Propagated staking storage and transfer database failures as fatal execution errors through
  the dynamic precompile adapter, preventing catchable reverts and transaction commits on
  unavailable state. Requires the matching fatal-error adapter support in `monad-revm`.

### Changed

- Propagate `MonadHaltReason` through `Evm` and `EvmFactory`, retaining the distinct reserve-balance
  rejection outcome from `monad-revm`. Downstream result types and halt-reason matches must use
  the re-exported Monad type, with ordinary Ethereum halts wrapped in its `Ethereum` variant.
  Pin `monad-revm` commit `5adee66553a1ed1644ad243aa94fde09739bd0d1`, including reserve
  enforcement, inspector completion, delegated precompiles, fatal database errors, and
  charged transaction gas.
- Root inspector completion callbacks now observe reserve rejection, including immediate
  empty-account and precompile results. Stock Geth and Parity traces use their generic revert
  labels; the returned execution result retains the distinct Monad reason. Attempted trace
  logs follow the stock tracer's ordinary revert behavior and are not committed logs.

### Added

- Integration coverage for stock Geth and Parity traces, and isolated internal calls that
  preserve the enclosing reserve tracker. A final unpaid deficit rolls back both isolated
  and enclosing execution; repayment and reverted isolated dips can complete successfully.
  Foundry adoption is a separate downstream change.

## [0.8.0] - 2026-09-09

### Compatibility

| Dependency | 0.7.0 | 0.8.0 |
|------------|-------|-------|
| `alloy-evm` | 0.38.0 | 0.39.0 |
| `alloy-primitives` | 1.6.1 | 1.6.1 |
| `revm` | 42.0.1 | 43.0.0 |
| `monad-revm` | 0.7.0 | 0.8.0 |

The Rust MSRV remains 1.94.1.

### Changed

- Updated to the published `monad-revm` 0.8.0 release and the matching Alloy EVM 0.39 and
  REVM 43 dependency stack.
- Updated the lockfile, README compatibility table, and installation examples for the release.

### Breaking changes and migration

- Applications that depend directly on both Monad crates should update `alloy-monad-evm` and
  `monad-revm` to 0.8.0 together.
- Downstream code must use Alloy EVM 0.39 and REVM 43 with compatible EVM, database, inspector,
  and precompile types.

## [0.7.0] - 2026-08-25

### Compatibility

| Dependency | 0.6.0 | 0.7.0 |
|------------|-------|-------|
| `alloy-evm` | 0.38.0 | 0.38.0 |
| `alloy-primitives` | 1.6.1 | 1.6.1 |
| `revm` | 42.0.1 | 42.0.1 |
| `monad-revm` | 0.6.0 | 0.7.0 |

The Rust MSRV remains 1.94.1.

### Added

- Added `MonadTen` support across Alloy EVM construction, runtime precompile selection, and
  hardfork transitions.
- Added factory-level execution coverage for MIP-8 page-level storage warming.

### Changed

- Updated to the published `monad-revm` 0.7.0 release and removed the temporary Git override.

### Migration

- Applications that depend directly on both crates should update `alloy-monad-evm` and
  `monad-revm` to 0.7.0 together.

## [0.6.0] - 2026-08-14

### Compatibility

| Dependency | 0.5.0 | 0.6.0 |
|------------|-------|-------|
| `alloy-evm` | 0.37.0 | 0.38.0 |
| `alloy-primitives` | 1.6.0 | 1.6.1 |
| `revm` | 41.0.0 | 42.0.1 |
| `monad-revm` | 0.5.1 | 0.6.0 |

The Rust MSRV is raised from 1.91 to 1.94.1.

### Changed

- Migrated the EVM factory, execution, inspection, and precompile integration to the Alloy 0.38,
  REVM 42, and `monad-revm` 0.6 interfaces.
- Hardened CI and release workflows with pinned actions, least-privilege permissions, dependency
  update checks, workflow security scanning, and crates.io trusted publishing.

### Validation

- Added regression coverage confirming that inspected regular transactions produce the same
  execution result and state changes as uninspected transactions while invoking the inspector.

### Breaking changes and migration

- Downstream code must use the Alloy 0.38, REVM 42, and `monad-revm` 0.6 trait surfaces and
  compatible transaction, environment, inspector, and precompile types.
- Building this release requires Rust 1.94.1 or newer.

## [0.5.0] - 2026-08-06

### Compatibility

| Dependency | 0.4.0 | 0.5.0 |
|------------|-------|-------|
| `alloy-evm` | 0.33.2 | 0.37.0 |
| `alloy-primitives` | 1.5.2 | 1.6.0 |
| `revm` | 38.0.0 | 41.0.0 |
| `monad-revm` | 0.4.0 | 0.5.1 |

The Rust MSRV remains 1.91.

### Added

- Added `no_std` support backed by `alloc`, including bare-metal checks with and without the
  `memory_limit` feature.
- Added inspected system-call execution so tracing remains active for Monad protocol calls.
- Added runtime precompile selection for per-frame Monad hardfork transitions.

### Changed

- Migrated the EVM factory, execution, inspection, and precompile integration to the Alloy 0.37
  and REVM 41 interfaces.
- Made the live `PrecompilesMap` authoritative for dispatch, membership, address enumeration, and
  warm-address tracking.
- Preserved injected precompiles and custom staking or reserve-balance replacements across
  hardfork transitions.
- Marked runtime-selected protocol precompiles as non-cacheable because their results and gas can
  depend on the active frame specification.
- Made `extend_monad_precompiles_for_spec` preserve existing custom entries at Monad protocol
  addresses.

### Fixed

- Fixed stale protocol precompile selection after MonadEight/MonadNine transitions.
- Fixed custom staking and reserve-balance overrides being bypassed by context-aware dispatch.
- Fixed removed precompiles remaining reported as present or warm.
- Fixed explicit reserve-balance removals being reinstalled after a hardfork round trip.
- Fixed temporary MonadEight reserve overrides preventing native MonadNine activation after the
  override was removed.

### Breaking changes and migration

- `MonadPrecompilesMap` is no longer `Clone`, matching the mutable `PrecompilesMap` integration in
  Alloy 0.37.
- Downstream code must use the Alloy 0.37 and REVM 41 trait surfaces and compatible transaction,
  environment, inspector, and precompile types.
- Custom replacements for staking or reserve balance must use identifiers other than the reserved
  `MonadStaking` and `MonadReserveBalance` identifiers.
- The native reserve-balance entry cannot be moved to another address because its execution needs
  the Monad journal context. Replace it with a custom precompile before moving it.

[0.8.0]: https://github.com/category-labs/alloy-monad-evm/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/category-labs/alloy-monad-evm/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/category-labs/alloy-monad-evm/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/category-labs/alloy-monad-evm/compare/v0.4.0...v0.5.0
