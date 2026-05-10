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
- [ ] `streamOptimized` (DEFLATE-compressed grains used by OVF)
- [ ] `vmfs` / `vmfsSparse` (ESXi-native; rarely seen outside ESXi)

Variants other than `monolithicSparse` return a clear "unsupported"
error rather than misreading the image.

## Layout

```
src/
  lib.rs         public API
  error.rs       Error / Result
  header.rs      512-byte sparse extent header (magic 'KDMV')
  descriptor.rs  embedded text descriptor parser
  reader.rs      VmdkReader — open, BlockRead/BlockDevice impls
  capi.rs        C ABI returning FsCoreDevice handles
tests/
  synthetic.rs   hand-built fixtures
include/
  vmdk.h         C ABI header
```

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

## License

MIT.
