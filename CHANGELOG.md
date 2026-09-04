# Changelog

Notable changes to `am-img-vmdk`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.


## [Unreleased]

## [0.3.4] — 2026-09-04

### Changed

- **One grain walk.** The grain-table traversal existed in several copies; it
  is now written once.
- **The header field offsets have names**, and the tests read the same ones
  rather than repeating the numbers.

### Fixed

- Fixtures clean up after a panicking test instead of leaving temp images
  behind.

## [0.3.3] — 2026-08-29

### Fixed

- **An image that declares a parent is refused rather than read as if it had
  none.** A VMDK with a parent is only half the data; reading it standalone
  returns whatever the child happens to contain and silently invents the rest.

### Added

- `chore` tasks own this crate's build, and the code-review report is recorded
  in the repo.
- The github-guard hook set replaces the hand-rolled pre-commit hooks.

## [0.3.2] — 2026-06-21

### Changed

- The publish job clones its path-dependency siblings, pinned to a tag rather
  than tracking a branch, and publishing is gated on the disk-image validator
  cross-check. A release built from a floating dependency is not reproducible.

## [0.3.1] — 2026-06-09

### Changed

- Pinned toolchain moves from 1.94.1 to 1.95.0, in lockstep with the rest of
  the family. A straggler links two copies of `_rust_eh_personality` into any
  consumer that binds both.

## [0.3.0] — 2026-06-01

### Added

- Cross-validation against an external disk-image validator.
- Unit tests for header and descriptor parsing; reader corruption and
  write-persistence tests.

## [0.2.0] — 2026-05-12

### Added

- Device-backed reader and the `monolithicSparse` write path.

### Added

- Release-on-tag pipeline using trusted publishing, and CI (test, fmt, clippy).

### Changed

- `am-fs-core` dependency moves to 0.2.

[Unreleased]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.3.4...HEAD
[0.3.4]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/antimatter-studios/rust-img-vmdk/releases/tag/v0.2.0
