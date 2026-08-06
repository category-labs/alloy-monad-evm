# Changelog

All notable changes to this project are documented in this file.

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

[0.5.0]: https://github.com/category-labs/alloy-monad-evm/compare/v0.4.0...v0.5.0
