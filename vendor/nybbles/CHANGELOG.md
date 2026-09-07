# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.8](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.8) - 2026-02-03

### Bug Fixes

- Remove avx2 runtime detection ([#53](https://github.com/alloy-rs/nybbles/issues/53))

### Dependencies

- Bump MSRV to 1.88 ([#48](https://github.com/alloy-rs/nybbles/issues/48))
- Bump codspeed

### Features

- Add byte_len ([#50](https://github.com/alloy-rs/nybbles/issues/50))

### Miscellaneous Tasks

- [meta] Add CODEOWNERS

### Other

- Update to tempoxyz ([#44](https://github.com/alloy-rs/nybbles/issues/44))

### Performance

- SIMD common prefix ([#49](https://github.com/alloy-rs/nybbles/issues/49))
- Clean up slice fn ([#51](https://github.com/alloy-rs/nybbles/issues/51))
- Cmp SIMD ([#46](https://github.com/alloy-rs/nybbles/issues/46))
- Eq as [u64; 5] ([#47](https://github.com/alloy-rs/nybbles/issues/47))
- Hash as [u64; 5] ([#45](https://github.com/alloy-rs/nybbles/issues/45))

### Refactor

- Restructure benches to be like ruint ([#52](https://github.com/alloy-rs/nybbles/issues/52))

## [0.4.7](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.7) - 2026-01-07

### Features

- Add new methods ([#43](https://github.com/alloy-rs/nybbles/issues/43))

### Miscellaneous Tasks

- Release 0.4.7

## [0.4.6](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.6) - 2025-09-29

### Dependencies

- Fix CI errors ([#39](https://github.com/alloy-rs/nybbles/issues/39))

### Features

- Implement FromStr ([#38](https://github.com/alloy-rs/nybbles/issues/38))

### Miscellaneous Tasks

- Release 0.4.6

## [0.4.5](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.5) - 2025-09-17

### Bug Fixes

- Nibbles deserialize ([#37](https://github.com/alloy-rs/nybbles/issues/37))

### Miscellaneous Tasks

- Release 0.4.5

## [0.4.4](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.4) - 2025-09-09

### Bug Fixes

- Serde serialize/deserialize ([#36](https://github.com/alloy-rs/nybbles/issues/36))

### Miscellaneous Tasks

- Release 0.4.4

## [0.4.3](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.3) - 2025-08-15

### Bug Fixes

- `pack_to_unchecked` on big-endian targets ([#35](https://github.com/alloy-rs/nybbles/issues/35))

### Miscellaneous Tasks

- Release 0.4.3

## [0.4.2](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.2) - 2025-08-12

### Features

- Add iterator for Nibbles ([#34](https://github.com/alloy-rs/nybbles/issues/34))

### Miscellaneous Tasks

- Release 0.4.2

## [0.4.1](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.1) - 2025-07-07

### Bug Fixes

- Bump minimum ruint version ([#30](https://github.com/alloy-rs/nybbles/issues/30))

### Features

- Add back support for big-endian targets ([#32](https://github.com/alloy-rs/nybbles/issues/32))

### Miscellaneous Tasks

- Release 0.4.1

## [0.4.0](https://github.com/alloy-rs/nybbles/releases/tag/v0.4.0) - 2025-06-19

### Dependencies

- More benchmarks ([#20](https://github.com/alloy-rs/nybbles/issues/20))

### Features

- Add missing benchmarks for performance-critical methods ([#22](https://github.com/alloy-rs/nybbles/issues/22))

### Miscellaneous Tasks

- Release 0.4.0

### Other

- More cases for slice ([#21](https://github.com/alloy-rs/nybbles/issues/21))
- Add codspeed ([#18](https://github.com/alloy-rs/nybbles/issues/18))

### Performance

- `U256` representation ([#17](https://github.com/alloy-rs/nybbles/issues/17))

### Styling

- Add proptests ([#26](https://github.com/alloy-rs/nybbles/issues/26))

## [0.3.4](https://github.com/alloy-rs/nybbles/releases/tag/v0.3.4) - 2025-01-06

### Miscellaneous Tasks

- Release 0.3.4
- Better debug impl ([#16](https://github.com/alloy-rs/nybbles/issues/16))

### Other

- Fix
- Update

## [0.3.3](https://github.com/alloy-rs/nybbles/releases/tag/v0.3.3) - 2024-12-30

### Features

- Add from_repr

### Miscellaneous Tasks

- Release 0.3.3

## [0.3.2](https://github.com/alloy-rs/nybbles/releases/tag/v0.3.2) - 2024-12-30

### Features

- Expose smallvec_with

### Miscellaneous Tasks

- Release 0.3.2

## [0.3.1](https://github.com/alloy-rs/nybbles/releases/tag/v0.3.1) - 2024-12-30

### Features

- Add more raw APIs for working with slices directly ([#15](https://github.com/alloy-rs/nybbles/issues/15))

### Miscellaneous Tasks

- Release 0.3.1

### Other

- Remove outdated ci

## [0.3.0](https://github.com/alloy-rs/nybbles/releases/tag/v0.3.0) - 2024-12-10

### Bug Fixes

- Ci branch target ([#13](https://github.com/alloy-rs/nybbles/issues/13))

### Dependencies

- Remove path encoding ([#12](https://github.com/alloy-rs/nybbles/issues/12))

### Features

- Improve benchmarks, compare with naive implementation ([#11](https://github.com/alloy-rs/nybbles/issues/11))

### Miscellaneous Tasks

- Release 0.3.0
- Sync cliff.toml

### Other

- Move deny to ci ([#14](https://github.com/alloy-rs/nybbles/issues/14))

## [0.2.1](https://github.com/alloy-rs/nybbles/releases/tag/v0.2.1) - 2024-02-26

### Documentation

- Add CHANGELOG.md and cliff.toml ([#10](https://github.com/alloy-rs/nybbles/issues/10))

### Features

- Import nybbles bench from reth
- Add SizeRange to proptest Arbitrary parameters

### Miscellaneous Tasks

- Release 0.2.1
- Add changelog script

## [0.2.0](https://github.com/alloy-rs/nybbles/releases/tag/v0.2.0) - 2024-02-26

### Bug Fixes

- Add an overflow check in unpack_heap ([#6](https://github.com/alloy-rs/nybbles/issues/6))
- Out-of-bound memory read on `Nibbles::get_byte` ([#4](https://github.com/alloy-rs/nybbles/issues/4))

### Features

- Make it clear when an invalid instance is created ([#8](https://github.com/alloy-rs/nybbles/issues/8))

### Miscellaneous Tasks

- Release 0.2.0
- Simplify slice implementation ([#7](https://github.com/alloy-rs/nybbles/issues/7))
- Update release.toml
- Update configs

### Other

- Add concurrency ([#9](https://github.com/alloy-rs/nybbles/issues/9))

### Performance

- Optimize usize::MAX check in get_byte ([#5](https://github.com/alloy-rs/nybbles/issues/5))

### Testing

- `arbitrary` feature separation ([#3](https://github.com/alloy-rs/nybbles/issues/3))

## [0.1.2](https://github.com/alloy-rs/nybbles/releases/tag/v0.1.2) - 2023-12-20

### Bug Fixes

- Assume macro

### Documentation

- Add logo

### Features

- More methods, examples

### Miscellaneous Tasks

- Release 0.1.2
- Rename macros

### Other

- Merge pull request [#1](https://github.com/alloy-rs/nybbles/issues/1) from dvush/pop-idx-mut
- Replace mut index with set_at
- Add mutable indexing, pop

## [0.1.1](https://github.com/alloy-rs/nybbles/releases/tag/v0.1.1) - 2023-12-15

### Features

- Implement Index

### Miscellaneous Tasks

- Release 0.1.1
- Add clippy.toml
- Add release.toml

### Other

- Master

## [0.1.0](https://github.com/alloy-rs/nybbles/releases/tag/v0.1.0) - 2023-12-15

### Documentation

- Add more examples
- Description

### Features

- Initial implementation

### Other

- Initial commit

### Performance

- Remove useless branch on smallvec heap init

<!-- generated by git-cliff -->
