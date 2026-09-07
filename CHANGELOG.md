# Changelog

Notable changes to `am-img-vmdk`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.


## [Unreleased]

### Fixed

- **A split-sparse extent is unsupported, not corrupt.** Each extent of a
  split disk carries a sparse header whose descriptor region is reserved
  and left empty, because the descriptor lives in the sidecar `.vmdk`.
  The parser saw a run of NULs, found no `createType`, and answered
  `Corrupt` about a healthy and complete file. The two verdicts mean
  opposite things to a caller — one invites a warning, a repair, or a
  refusal to trust the disk; the other says to use a different reader —
  so an empty descriptor region is now read as the positive signal it is.
  A region with *content* that fails to parse is still `Corrupt`.

### Fixed

- **A sparse-extent revision this crate cannot read is refused by its
  version.** The header's format revision was parsed and never compared
  to anything, so an image declaring any revision at all was read with
  version-1 semantics. Version 3 — the stream-optimized layout, with
  compressed grains, grain markers and a footer replacing the header —
  was refused only by a *second* field, `compressAlgorithm`, which leaves
  the refusal resting on the one place the format states the fact twice;
  an image making only the first statement walked through. Versions 1 and
  2 are both accepted: `qemu-img` writes 2 for a monolithic sparse extent
  carrying the zeroed-grain marker, which this crate reads.
- **An ESXi `vmfsSparse` extent is refused as `vmfsSparse`, not as "not a
  VMDK".** Its magic is `COWD` rather than `KDMV`, so it failed the magic
  test — the same wrong verdict the flat layouts used to get, by a
  different route. The descriptor parser has carried a stable
  `vmfsSparse` message all along that nothing could reach.

### Added

- `header::SUPPORTED_VERSIONS` and `header::MAGIC_VMFS_SPARSE`.

### Fixed

- **A one-sector write no longer allocates whatever `num_gtes_per_gt`
  asks for.** The read path bounds a grain table against the file before
  loading it; the write path *creates* one, so nothing preceded it, and
  it sized both the allocation and its zero-fill buffer straight from the
  header field. With `num_gtes_per_gt` patched to `0x0100_0000`, a
  512-byte write into a 69,120-byte image produced a 67,243,520-byte file
  and a 64 MiB zero buffer. The field is now capped at parse time, which
  covers both paths.

- **A flat or split VMDK is refused by its create type, not denied to be
  a VMDK.** The file a user is handed for `monolithicFlat`,
  `twoGbMaxExtentFlat` or `twoGbMaxExtentSparse` is a few hundred bytes
  of descriptor text with no `KDMV` magic anywhere in it, so the
  sparse-header parse failed first and reported "corrupt" or "not a
  VMDK". Both are false, and false in the direction that leads a probing
  caller to move on and tell the user their VMDK is unrecognised. `open`
  now reads the head of the file first and, when it is a descriptor,
  returns `Unsupported` naming the create type. Only a file that is
  neither a sparse extent nor a parseable descriptor is `NotVmdk`.

### Fixed

- **Writes keep the redundant grain directory in step with the primary.**
  A sparse VMDK carries a second copy of the grain directory and of every
  grain table — `rgd_offset`, announced by bit 1 of `flags`, and present
  on every image qemu and VMware produce. It was parsed and never used,
  so after a write the two copies disagreed: the primary said a grain was
  at sector N and the redundant copy still said it was absent. Nothing
  reported it, because `qemu-img check` does not consult the redundant
  tables — and the fallback read those tables exist for then returned a
  hole where the data was. A grain table this crate allocated was worse
  than stale: it was missing from the redundant directory entirely.

### Changed

- `open_rw` refuses an image whose redundant grain directory does not fit
  inside the file. A read-only open still succeeds, since reads never
  consult it.

### Added

- The `FLAG_REDUNDANT_GRAIN_TABLE` constant.

### Fixed

- **Concurrent writes no longer lose data while reporting success.**
  Every structure had its own lock and none was held across the
  test-then-allocate-then-publish sequence that allocation is, so two
  threads writing into the same absent grain table — or the same sparse
  grain — could both allocate, and the second could overwrite the
  first's published pointer. Both calls returned `Ok(())`; one writer's
  bytes were left in a grain nothing referenced, and the offset read
  back as zeros. Allocation is now serialised as a whole, while a write
  into a grain that already exists still runs unserialised.
- **The grain-table cache is keyed by the table, not the slot.** It
  recorded which directory *index* was cached, so an update meant for
  one table could be applied to a different table's cached contents.

### Fixed

- **A zeroed grain reads as zeros, not as the descriptor.** A grain-table
  entry of `1` is the format's zeroed-grain marker — "present and entirely
  zero" — announced by bit 2 of the header's `flags`. That flags word was
  parsed and never read, so the marker was followed as if it were host
  sector 1, which is where the embedded descriptor lives: a region the
  guest had zeroed came back as the image's own ASCII configuration text,
  with no error. Writing into such a grain landed the payload on the
  descriptor and left a file `qemu-img` could no longer open.
- **A grain pointer into the image's own metadata is refused.** The grain
  *table* pointer had been bounds-checked since it was written; the grain
  pointer was checked only by the read failing past EOF. A pointer that
  lands inside the header, the descriptor or a grain directory is now an
  error rather than plausible-looking bytes.

### Added

- `SparseHeader::uses_zeroed_grain_marker`, plus the `FLAG_ZEROED_GRAIN`
  and `GTE_ZEROED_GRAIN` constants that name the two halves of the
  convention.

## [0.3.5] — 2026-09-06

### Fixed

- A grain size that multiplies out to zero is refused. The descriptor
  states a grain in sectors; zero, or a value that wraps when turned
  into bytes, made the first read divide by zero.

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
