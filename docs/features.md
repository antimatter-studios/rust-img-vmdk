# Features

What this crate does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** until the next one. **Tracking** names the issue for
anything not finished.

States:

- **Supported**: works, and is checked against `qemu-img`.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `monolithicSparse`: header, embedded descriptor, grain directory, grain tables, grain data | Supported | 0.2.0 | | `synthetic.rs`, `qemu_validation.rs` |
| The redundant grain directory, used only when the header declares it | Supported | 0.4.0 | | `synthetic.rs`, `qemu_validation.rs` |
| Zeroed grains read as zeros | Supported | 0.4.0 | | `qemu_validation.rs` |
| `streamOptimized`: DEFLATE grains inflated through their markers, the grain directory from the header or the footer | Supported, read-only | 0.4.0 | | `synthetic.rs`, `qemu_validation.rs`, `tests/cli/test-read.sh` |
| A grain or grain-table pointer into the image's own metadata | Refused | 0.4.0 | | `write.rs`, `corruption.rs` |
| A snapshot delta or linked clone (the descriptor names a parent) | Refused | 0.3.3 | | `corruption.rs` |
| `monolithicFlat`, `twoGbMaxExtentSparse`, `twoGbMaxExtentFlat` | Refused by their create type | 0.4.0 | | `synthetic.rs`, `qemu_validation.rs`, `tests/cli/test-unsupported.sh` |
| An ESXi `vmfsSparse` extent (`COWD` magic) | Refused by name | 0.4.0 | | `corruption.rs` |
| A sparse-extent revision this crate cannot read | Refused by its version | 0.4.0 | | `corruption.rs` |
| A text file naming a `createType` that is not a descriptor | Refused as not a VMDK | 0.4.0 | | `synthetic.rs`, `qemu_validation.rs` |
| Fuzzed sparse-header and descriptor parsers | Supported | 0.4.0 | | `fuzz_decoders.rs` |

## Writing

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `monolithicSparse`: writes to allocated grains; grains and grain tables allocated on write; data, grain table, grain directory, flush in that order | Supported | 0.2.0 | | `write.rs`, `qemu_validation.rs` |
| The redundant grain directory kept in step with the primary | Supported | 0.4.0 | | `write.rs` |
| A write changes the content identifier (`CID`) | Supported | 0.4.0 | | `write.rs` |
| `uncleanShutdown` set while writing and cleared on flush | Supported | 0.4.0 | | `write.rs`, `tests/cli/test-info.sh` |
| Concurrent writers | Supported | 0.4.0 | | `write.rs` |
| Opening a `streamOptimized` image read-write | Refused: a rewritten grain cannot go back where the old one was | 0.4.0 | | `tests/cli/test-write.sh` |
| Creating or resizing an image | Not supported | | | `tests/cli/test-unsupported.sh` |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Rust API (`VmdkReader`), over a path or any `rust-fs-core` device | Supported | 0.2.0 | | `synthetic.rs` |
| C ABI returning `FsCoreDevice` handles | Supported | 0.2.0 | | `header_names_the_built_library.rs` |
| `img.vmdk` `info`/`get`, `read`, `write` (`--features cli`) | Supported | 0.4.0 | | `cli_write.rs`, `tests/cli/test-info.sh`, `tests/cli/test-read.sh`, `tests/cli/test-write.sh` |
| `img.vmdk` `create`, `resize`, `set` | Not supported (`not implemented`, exit 3) | 0.4.0 | | `tests/cli/test-unsupported.sh` |
| `rust-img-vmdk doctor`, man pages, shell completions | Supported | 0.4.0 | | `tests/cli/test-names.sh`, `tests/cli/test-docs.sh` |
