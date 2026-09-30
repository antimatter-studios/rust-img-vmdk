# vmdk

Pure-Rust reader for the VMware VMDK (Virtual Machine Disk) format.
Implemented from VMware's published *Virtual Disk Format* technical
note; no GPL code is copied or linked. Exposes a Rust API and a C ABI
suitable for FFI from C/C++/Go/Swift.

## Status

- [x] `monolithicSparse` (single file: header + embedded descriptor +
      grain directory + grain tables + grain data)
- [x] `BlockRead` + `BlockDevice` impl via `am-fs-core` — generic over
      any device, not just files
- [x] C ABI: `vmdk_open` / `vmdk_open_rw` (path) and
      `vmdk_open_on_device` / `vmdk_open_rw_on_device` (existing
      `FsCoreDevice` handle)
- [x] Write support (monolithicSparse): write-through to allocated
      grains, allocate-on-write for sparse grains, allocate-on-write
      for grain tables themselves. Crash-safety order is data →
      grain-table → grain-directory (when growing) → flush.
- [ ] `monolithicFlat` (single contiguous data file, descriptor in a
      sidecar `.vmdk`)
- [ ] `twoGbMaxExtentSparse` / `twoGbMaxExtentFlat` (split-extent
      variants used for FAT32 hosts)
- [x] `streamOptimized` (DEFLATE-compressed grains used by OVF),
      **read-only**: grains are inflated through their markers, with the
      grain directory taken from the header or, for a stream written in
      one pass, the footer. Opening one read-write is refused -- a
      rewritten grain compresses to a different length and cannot go
      back where the old one was.
- [ ] `vmfs` / `vmfsSparse` (ESXi-native; rarely seen outside ESXi). A
      `vmfsSparse` extent carries the magic `COWD` rather than `KDMV` and
      is refused by name rather than reported as not a VMDK.

Variants other than `monolithicSparse` and `streamOptimized` return a clear "unsupported"
error rather than misreading the image. That includes the ones whose
file is a descriptor rather than a sparse extent -- `monolithicFlat` and
the `twoGbMaxExtent*` pair have no `KDMV` magic anywhere, and are
recognised by parsing the descriptor text and naming the `createType`.
Only a file that is neither a sparse extent nor a parseable descriptor
is reported as not a VMDK. So does a snapshot delta or
linked clone: it *is* `monolithicSparse`, but its descriptor names a
parent (`parentFileNameHint` / `parentCID`) and the grains it does not
own live in that parent, so reading it standalone would return zeros.

## Layout

```
src/
  lib.rs         public API
  error.rs       Error / Result
  header.rs      512-byte sparse extent header (magic 'KDMV')
  descriptor.rs  embedded text descriptor parser
  reader.rs      VmdkReader — open, BlockRead/BlockDevice impls
  capi.rs        C ABI returning FsCoreDevice handles
  cli/           img.vmdk / rust-img-vmdk, behind the `cli` feature
tests/
  synthetic.rs   hand-built fixtures
include/
  vmdk.h         C ABI header
```

## Command line

`img.vmdk <image> <verb>` reports and reads a VMDK image without a
hypervisor. It is one multi-call binary, `rust-img-vmdk`, with `img.vmdk` a
link to it; `rust-img-vmdk img ...` is the same program under the one name
nothing else on `PATH` can shadow, and `rust-img-vmdk doctor` says whether
the `img.vmdk` on `PATH` is this one. Build it with the `cli` feature (the
library alone gains no dependency from it):

```sh
chore cli:install                         # or: cargo build --release --features cli
img.vmdk disk.vmdk info                   # JSON; --text for people
img.vmdk disk.vmdk read -o disk.raw       # the whole virtual disk, as a raw image
img.vmdk disk.vmdk read --offset 0 --length 512 | xxd
```

Metadata is JSON by default, led by the keys every `img.<fmt>` tool shares
(`format`, `virtual_size`, `block_size`, `backing`, `dirty`) with the
format's own under `vmdk`. A failure is `{"error": "...", "code": N}` on
stderr, `N` being the exit status: 1 failed, 2 wrong command line, 3 not
implemented. It reads what the library reads, monolithicSparse and
streamOptimized; any other layout answers `not implemented` naming its
create type. `create`, `resize` and `set` exist and answer `not
implemented`: the library has no creator and no resize.

`chore test:cli` tests the tool as installed, against `qemu-img`.

## Spec

VMware's *Virtual Disk Format Specification* (publicly available from
VMware). Sparse-extent layout:

1. Sector 0: 512-byte `SparseExtentHeader` (little-endian, magic
   `0x564D444B` = `KDMV`).
2. Embedded descriptor text (sectors `descriptorOffset .. +descriptorSize`).
3. Redundant grain directory (`rgdOffset`, optional, ignored here).
4. Primary grain directory (`gdOffset`).
5. Grain tables (each `numGTEsPerGT` u32 entries).
6. Grain data (`grainSize` sectors per grain — typically 64 KiB).

A virtual sector `V` resolves as `gd[V / gs / GTEs][V / gs % GTEs] * 512`
plus `(V mod gs) * 512` byte offset within the grain.

## Verifying a release

From the next release onward, every version published to crates.io is
also attached to the GitHub release for its tag, with a build-provenance
attestation signed by this repository's release workflow. It proves the
crate was built by `.github/workflows/release.yml` from a commit in this
repository, not uploaded from someone's machine. To check the crates.io
download of version `X.Y.Z`:

```sh
curl -sSfLo am-img-vmdk-X.Y.Z.crate https://static.crates.io/crates/am-img-vmdk/am-img-vmdk-X.Y.Z.crate
gh attestation verify am-img-vmdk-X.Y.Z.crate \
  --repo antimatter-studios/rust-img-vmdk \
  --signer-workflow antimatter-studios/rust-img-vmdk/.github/workflows/release.yml
```

The workflow refuses to attest a `.crate` whose sha256 differs from the
checksum crates.io records for that version, so the file on the release
page and the crates.io download are the same bytes.

## License

MIT.
