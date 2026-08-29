# Human-Code Report — am-img-vmdk

**Date:** 2026-08-28
**Scope:** full crate (`src/*.rs`, `tests/*.rs`, `include/vmdk.h`, `chores.yml`)
**Phases run:** 0 (Understand), 1 (Scan & Triage), 3 (Report). Phase 2 / dev-loop **not** run.

> **THIS IS ANALYSIS ONLY. NO CODE WAS CHANGED.**
> The working tree is untouched apart from this file. Nothing was committed, no
> branch was created, and no test was added, modified, or deleted. Every
> "suggested fix" below is a proposal awaiting your confirmation, not a
> description of work already done.

## Counts

| | |
|---|---|
| Items found | **22** |
| Items fixed | **0** (report-only run) |
| Items skipped | **0** (nothing triaged out; the full list is below) |
| High severity | 4 |
| Medium severity | 12 |
| Low severity | 6 |

**Baseline test contract** (captured for a future dev-loop, `cargo test`):

| Suite | Tests | Result |
|---|---|---|
| `src/lib.rs` unit tests | 16 | pass |
| `tests/corruption.rs` | 7 | pass |
| `tests/synthetic.rs` | 7 | pass |
| `tests/write.rs` | 7 | pass |
| `tests/qemu_validation.rs` | 0 run / 6 defined | feature-gated off by default |
| **Total (default features)** | **37** | **37 pass, 0 fail** |

`cargo clippy --all-targets` is clean — zero warnings. So the smells below are all
things the compiler and the linter are happy with; they cost humans, not machines.

---

## The theme

The context for this review guessed that **naming would be the theme — whether VMDK's
variants are distinguished by name or by a chain of conditionals.** That guess is
correct, and it is the through-line of every High finding.

VMDK is a format with *two* orthogonal variant axes:

1. **Container shape** — `monolithicSparse`, `monolithicFlat`, `twoGbMaxExtentSparse`,
   `twoGbMaxExtentFlat`, `streamOptimized`, `vmfs`, `vmfsSparse`, `fullDevice`,
   `partitionedDevice`.
2. **Extent type** — `SPARSE`, `FLAT`, `ZERO`, `VMFS`, `VMFSSPARSE`, `VMFSRAW`, each
   with an access mode `RW` / `RDONLY` / `NOACCESS`.

Plus a third dimension nobody modelled at all: whether the disk is **standalone or a
delta/child** of a parent (see H4).

None of these three axes is a type in this crate. Axis 1 is a `String` compared against
a literal and then laundered through a nine-arm identity `match`. Axis 2 is two more
bare `String` fields, one of which is validated against a closed set written inline as a
`matches!` pattern and the other of which is not validated at all. Axis 3 does not exist.

The consequence is that the crate's central question — *"what kind of VMDK is this, and
can I read it?"* — is answered by scattered string comparisons rather than by a value you
can `match` on exhaustively. The compiler cannot tell you where the variant list is
incomplete, so the list has drifted into **six** hand-maintained copies (`descriptor.rs`,
`lib.rs`, `reader.rs`, `capi.rs`, `include/vmdk.h`, `README.md`).

---

# Findings

## High severity

### H1 — `createType` dispatch is a nine-arm identity `match`, not a type

- **Location:** `src/descriptor.rs:59-73` (the match); `src/descriptor.rs:31` (the `String` field);
  duplicate variant lists at `src/lib.rs:6-9`, `src/reader.rs:1-5`, `src/capi.rs:19-22`,
  `include/vmdk.h:26-31`, `README.md:12-24`
- **Category:** Misleading/opaque names + duplication + magic strings
- **Severity:** **High** — this is the crate's primary dispatch and it is untyped
- **Test coverage:** Good. `rejects_every_known_unsupported_variant_with_stable_message`
  (`src/descriptor.rs:152-169`) walks all eight rejected names;
  `unknown_create_type_maps_to_stable_message` (`:171-178`) covers the fallback;
  `parses_monolithic_sparse` (`:117-134`) covers the accepted one. A refactor here is
  well-guarded.

The match maps every input to a `&'static str` **spelled identically to the input**:

```rust
let msg: &'static str = match create_type.as_str() {
    "monolithicFlat"       => "monolithicFlat",
    "twoGbMaxExtentSparse" => "twoGbMaxExtentSparse",
    ...
    _ => "unknown createType",
};
return Err(Error::Unsupported(msg));
```

Eight of the nine arms are the identity function. The match's *only* real job is to
launder a heap `String` into a `&'static str` so it fits `Error::Unsupported(&'static str)`.
The domain knowledge it appears to encode — "these are the VMDK create types" — is
carried entirely by the spelling of the literals, so nothing enforces that the list is
right, complete, or consistent with the five other copies of it elsewhere in the tree.

**Suggested fix:** introduce a `CreateType` enum in `descriptor.rs` with one variant per
known type plus `Unknown`, a `FromStr`/`from_descriptor_value` constructor, a
`as_descriptor_str()` for the reverse, and an `is_supported()` predicate. `Descriptor`
holds `CreateType` instead of `String`. The `Error::Unsupported` message then comes from
`as_descriptor_str()`, the identity match evaporates, and adding `monolithicFlat` support
later becomes "add an arm to `is_supported`" with the compiler pointing at every other
site that needs to change. This also gives `lib.rs` / `capi.rs` / `vmdk.h` / `README.md`
one authoritative list to point at instead of four transcriptions.

**Do not change** the `Error::Unsupported(&'static str)` signature — it is part of the
public API and the tests assert on the exact strings.

---

### H2 — `read_at` and `write_at` carry two copies of the grain-address walk

- **Location:** `src/reader.rs:232-294` (`read_at`) and `src/reader.rs:331-411` (`write_at`)
- **Category:** Duplicated code + god function
- **Severity:** **High** — this is the crate's correctness core, duplicated
- **Test coverage:** Excellent on both sides. Reads: `allocated_grain_round_trip`,
  `unallocated_grain_reads_zero`, `whole_image_unallocated_reads_zero`,
  `read_past_end_errors` (`tests/synthetic.rs`). Writes: all 7 of `tests/write.rs` plus
  `write_into_sparse_grain_persists_across_reopen` (`tests/corruption.rs:197`). A
  refactor here is as well-guarded as it gets.

Diffing the first 35 lines of each function: **21 of 35 lines are byte-identical**. Both
compute, verbatim:

```rust
let end = offset.checked_add(len).ok_or(Error::Corrupt("offset+len overflow"))?;
if end > self.virtual_size { return Err(Error::OutOfBounds { offset, len, size: self.virtual_size }); }
let grain_bytes = self.grain_size_bytes();
let entries_per_gt = self.header.num_gtes_per_gt as u64;
...
    let in_grain   = cursor % grain_bytes;
    let grain_idx  = cursor / grain_bytes;
    let gt_idx     = (grain_idx / entries_per_gt) as usize;
    let gte_idx    = (grain_idx % entries_per_gt) as usize;
    let bytes_remaining_in_grain = grain_bytes - in_grain;
    let chunk_len  = std::cmp::min(bytes_remaining_in_grain, end - cursor) as usize;
    let gt_sector  = { let gd = self.gd.lock().unwrap();
                       if gt_idx >= gd.len() { return Err(Error::Corrupt("gt_idx past grain directory")); }
                       gd[gt_idx] };
```

The only differences are `&mut buf[..]` vs `&buf[..]`, the read-only guard, and what
happens *after* `gt_sector` is resolved.

**Note on the three-instance rule:** the human-code rules say don't extract a helper
below three instances. I am flagging this anyway, and here is the argument: this is not
"two callers happen to share four lines". This is the crate's virtual→host address
translation — the single piece of arithmetic that, if wrong, silently returns or writes
the wrong bytes — existing in two independently-maintainable copies. The extraction is
justified by *naming a concept the code currently has no word for*, not by line-count
savings. A `GrainSpan { grain_idx, gt_idx, gte_idx, in_grain, chunk_len }` produced by an
iterator (`fn grain_chunks(&self, offset, len) -> impl Iterator<Item = GrainSpan>`) makes
"walk the request grain by grain" a thing you can read, and leaves `read_at`/`write_at`
as short bodies that only express what they do differently.

---

### H3 — the parsed descriptor is discarded, and the doc comment says otherwise

- **Location:** `src/reader.rs:145` (`let _descriptor = Descriptor::parse(desc_text)?;`);
  the claim at `src/descriptor.rs:8-9`; the unread fields at `src/descriptor.rs:21-26`
- **Category:** Comment that lies + speculative/dead code
- **Severity:** **High** — the comment actively misleads about a safety property
- **Test coverage:** Partial. `Descriptor::parse` itself is well covered by the unit
  tests. The *reader's* use of it is covered only for the error path
  (`rejects_non_monolithic_sparse`, `tests/synthetic.rs:230`). Nothing covers the
  discarded extent data, because there is nothing to cover.

`descriptor.rs` opens with:

> "The extent line: `RW <sectors> SPARSE "<filename>"`. **We stash sector count and
> filename for sanity.**"

Nothing is stashed and nothing is sanity-checked. `reader.rs:145` binds the whole parsed
`Descriptor` to `_descriptor` and drops it on the next line. `Descriptor::parse` is called
purely for its side effect of returning `Err(Unsupported)` on a non-sparse image; the
`extents` vector it builds is constructed and thrown away on every single open.

A reader of `descriptor.rs` will reasonably believe the extent's declared sector count is
cross-checked against `header.capacity`. It is not. The two can disagree by any amount and
the image opens.

**It gets worse in combination with the silent-skip behaviour.** `parse_extent`
(`descriptor.rs:94-111`) returns `None` on any malformed line and `Descriptor::parse`
silently `continue`s past it — a behaviour deliberately pinned by
`ignores_malformed_extent_line_with_non_numeric_sectors` (`descriptor.rs:207-216`), whose
own comment says "the line is silently ignored, leaving zero extents". So a descriptor
whose extent line is corrupt parses to a `Descriptor` with an empty `extents` vec, which
the reader then discards anyway. There is no path by which a corrupt extent line is ever
reported.

**Suggested fix, two options — your call which:**
(a) Honour the comment: keep the `Descriptor`, and validate `extents[0].sectors` against
`header.capacity` and `extents[0].kind == SPARSE` at open time, rejecting mismatches as
`Corrupt`. This is the behaviour the docs already promise.
(b) Delete the claim: change the doc comment to say the extent line is parsed for API
consumers but not used internally, and rename `_descriptor` to something that says why
(`// parsed for its variant check only`).
Option (a) is more work and changes behaviour (some real-world images may have benign
mismatches); option (b) is a pure documentation fix. I would not do (a) without a
qemu-validation run first.

---

### H4 — delta / linked-clone disks are indistinguishable from full disks

- **Location:** `src/descriptor.rs:54` (`// Ignore everything else (ddb.*, encoding, version, CID, ...)`);
  the absent `parentFileNameHint` / `parentCID` handling
- **Category:** Speculative-absence — a variant axis that is not modelled at all
- **Severity:** **High** — correctness-adjacent, surfaced by the naming gap
- **Test coverage:** **None.** No test constructs a descriptor with `parentFileNameHint`.

A VMware snapshot delta (and a linked clone) is a `monolithicSparse` VMDK. Its
`createType` is exactly the string this crate accepts. What distinguishes it is
`parentFileNameHint="base.vmdk"` and a `parentCID` other than `ffffffff` — both of which
`Descriptor::parse` explicitly discards, with `CID` named in the ignore-list comment.

The result: hand this crate a snapshot delta and it opens successfully and reports the
full virtual size, but every grain the delta does not own reads as **zeros** instead of
the parent's data. That is a silent wrong-data read, which is precisely the failure mode
the module docs promise to avoid ("Variants other than `monolithicSparse` return a clear
'unsupported' error rather than misreading the image", `README.md:26`).

This is on the list because it is a *consequence of the naming problem*, not a separate
issue: because "what kind of VMDK is this" is answered by one string equality rather than
by a modelled type, the parent-linkage axis was never given a place to live and so was
never considered.

**Suggested fix:** parse `parentFileNameHint` (and optionally `parentCID`) in
`Descriptor`, and have `open_inner` reject a descriptor carrying a parent hint with
`Error::Unsupported("delta/child disk with parent")`. That is a small, cheap change that
converts a silent wrong-read into the clear error the docs already claim. Add a
descriptor unit test and a `tests/corruption.rs`-style open test. **This is a behaviour
change, not a refactor** — it belongs in its own commit, and arguably its own PR, rather
than inside a readability pass.

---

## Medium severity

### M1 — `Extent.access` and `Extent.kind` are bare `String` over closed sets

- **Location:** `src/descriptor.rs:20-26` (struct), `src/descriptor.rs:94-111` (`parse_extent`)
- **Category:** Misleading/opaque names
- **Severity:** Medium
- **Test coverage:** `parses_readonly_and_noaccess_extent_access_modes`
  (`descriptor.rs:196-205`) covers the access modes. **Nothing covers `kind`.**

The closed set for `access` is already *known* to the code — it is written inline as a
pattern at `descriptor.rs:97`:

```rust
if !matches!(access, "RW" | "RDONLY" | "NOACCESS") { return None; }
```

— it is just not named. `kind` has a closed set too (`SPARSE`, `FLAT`, `ZERO`, `VMFS`,
`VMFSSPARSE`, `VMFSRAW`) and is not validated at all: `RW 2048 BANANA "x.vmdk"` parses
into a perfectly happy `Extent`.

Same fix shape as H1 — `ExtentAccess` and `ExtentKind` enums. Lower severity than H1 only
because the parsed extents are currently discarded (H3), so the weak typing does no
damage *yet*. It will the moment H3 option (a) is taken or `monolithicFlat` support lands.

### M2 — the GD/GT entry size `4` is an unnamed literal at seven sites

- **Location:** `src/reader.rs:163, 173, 302, 306, 461, 482, 510`
- **Category:** Magic numbers
- **Severity:** Medium
- **Test coverage:** Every read and write test exercises these lines.

```
reader.rs:163   let gd_byte_len = (gt_count * 4) as usize;
reader.rs:173   for chunk in gd_bytes.chunks_exact(4) {
reader.rs:302   let mut bytes = vec![0u8; entries_per_gt * 4];
reader.rs:306   for chunk in bytes.chunks_exact(4) {
reader.rs:461   let gt_bytes = entries_per_gt * 4;
reader.rs:482   let gd_entry_off = self.header.gd_offset * SECTOR_SIZE + (gt_idx as u64) * 4;
reader.rs:510   let entry_off = (gt_sector as u64) * SECTOR_SIZE + (gte_idx as u64) * 4;
```

All seven mean the same thing: *a grain-directory or grain-table entry is a
little-endian u32*. Five more copies live in the test fixtures (`synthetic.rs:111`,
`write.rs:99`, `write.rs:163`, `write.rs:172`, `corruption.rs:105`).

Per the skill's centralisation rule, this is a family: `SECTOR_SIZE`, the entry size, and
`HEADER_SIZE` are all "on-disk layout constants" and belong together — either a
`constants.rs` or a documented block at the top of `reader.rs`. Suggested:
`const GTE_SIZE: u64 = 4;` with a one-line comment naming it as the u32 sector pointer.

### M3 — `SparseHeader::parse` field offsets are bare numerals, duplicated four ways

- **Location:** `src/header.rs:53-68`; duplicated in `tests/synthetic.rs:60-76`,
  `tests/write.rs:58-71`, `tests/write.rs:130-143`, `tests/corruption.rs:75-83`
- **Category:** Magic numbers
- **Severity:** Medium
- **Test coverage:** Good — seven header unit tests, one per rejection path.

```rust
let capacity          = read_u64(bytes, 12);
let grain_size        = read_u64(bytes, 20);
...
let unclean_shutdown  = bytes[72];
let compress_algorithm = read_u16(bytes, 77);
```

`77` is the single most load-bearing unnamed number in the crate: it is where the
compression flag lives, and reading it wrong means silently accepting a `streamOptimized`
image. The offsets are only decodable against the ASCII table in the module doc
(`header.rs:6-25`) — good documentation, but documentation is not the same as a name.

**The need has already been felt once and half-met:** `tests/corruption.rs:36-39` defines

```rust
const OFF_DESC_OFFSET: u64 = 28;
const OFF_DESC_SIZE:   u64 = 36;
const OFF_GD_OFFSET:   u64 = 56;
```

— three of the offsets, named, in a test file, invisible to the parser that also needs
them. Centralise the whole family as `pub(crate)` consts in `header.rs` and let the tests
import them.

### M4 — the three test-fixture builders are near-verbatim triplicates

- **Location:** `tests/synthetic.rs:26-135`, `tests/write.rs:24-159`, `tests/corruption.rs:25-118`
- **Category:** Duplicated code
- **Severity:** Medium
- **Test coverage:** N/A — this *is* the test code.

Measured duplication:

| Item | Copies | Locations |
|---|---|---|
| `build_header()` | 4 | `synthetic.rs:58`, `write.rs:56`, `write.rs:130` (inline), `corruption.rs:73` |
| descriptor-sector builder | 3 | `synthetic.rs:80`, `write.rs:75`, `corruption.rs:87` |
| Layout constants block | 3 | `synthetic.rs:49-56`, `write.rs:28-35`, `corruption.rs:26-33` |
| `tmp_path` / `tmp` | 4 | `synthetic.rs:28`, `write.rs:37`, `corruption.rs:41`, `qemu_validation.rs:49` |
| `trait WriteAt` + impl | 2 | `synthetic.rs:37`, `write.rs:46` |
| `struct TempPath` RAII | 2 | `corruption.rs:55`, `qemu_validation.rs:60` |

Every one of these clears the three-instance bar except `WriteAt` and `TempPath`.
`tests/common/mod.rs` is the idiomatic Rust home. `write.rs:26-27` even carries a comment
acknowledging the problem — *"Same layout constants the read-side fixture uses, so the two
test files stay calibrated to each other"* — which is a comment doing a shared module's job.

### M5 — two of four test files leak fixture files when an assertion panics

- **Location:** `tests/corruption.rs:55-71` and `tests/qemu_validation.rs:60-76` have
  `TempPath`; `tests/synthetic.rs` and `tests/write.rs` do not
- **Category:** Duplicated-but-divergent code
- **Severity:** Medium
- **Test coverage:** N/A

`synthetic.rs` and `write.rs` clean up with a trailing `let _ = std::fs::remove_file(&path);`
at the end of each test — which does not run if an assertion above it panics. `corruption.rs`
and `qemu_validation.rs` solved this with an identical RAII `TempPath` struct, each with an
identical doc comment explaining why ("so a panicking assertion can't leak fixtures into the
temp dir across CI runs"). The fix was written twice and applied to half the suite.

Folding `TempPath` into the `tests/common/mod.rs` from M4 fixes M4 and M5 together and
removes 14 trailing `remove_file` lines.

### M6 — `open_inner` is an 87-line, six-phase god function

- **Location:** `src/reader.rs:111-198`
- **Category:** God function + mixed abstraction levels
- **Severity:** Medium
- **Test coverage:** Excellent — `tests/corruption.rs` hits five of its six failure
  branches (`missing_descriptor_offset_is_unsupported`,
  `descriptor_extending_past_eof_is_corrupt`, `zero_grain_directory_offset_is_corrupt`,
  `grain_directory_past_eof_is_corrupt`, `non_utf8_descriptor_is_corrupt`).

Six distinct phases with six separate error vocabularies, separated by blank lines and
section comments — the classic tell:

1. `:112-115` device-size floor check
2. `:117-121` read + parse the 512-byte header
3. `:123-145` descriptor: presence check → offset arithmetic → EOF bounds → read → UTF-8 → parse → **discard**
4. `:147-157` grain-directory geometry (`grains_total`, `gt_count`, u32 range check)
5. `:159-175` grain-directory: offset arithmetic → EOF bounds → read → LE decode loop
6. `:177-197` virtual size, alloc cursor, struct construction

Phases 3, 4 and 5 are each a nameable operation (`read_and_validate_descriptor`,
`grain_directory_geometry`, `load_grain_directory`). Splitting them makes the failure
taxonomy legible: right now you have to read all 87 lines to answer "what makes an open
fail?"

### M7 — `write_at`'s per-chunk body mixes three abstraction levels at four levels of nesting

- **Location:** `src/reader.rs:366-405`
- **Category:** Deep nesting + mixed abstraction levels
- **Severity:** Medium
- **Test coverage:** Excellent (all of `tests/write.rs`).

Inside `while cursor < end` you have, in sequence: a mutex-guarded GD lookup, a
conditional GT allocation, a grain lookup, a conditional grain allocation, inside which
sits a *further* conditional partial-write zero-fill, and finally raw
`(sector as u64) * SECTOR_SIZE + in_grain` byte arithmetic — high-level allocation policy
and low-level offset math in the same twenty lines.

The `if in_grain != 0 || (chunk_len as u64) < grain_bytes` condition at `:388` is the
densest expression in the crate: it means "this write does not cover the whole grain", and
it takes real effort to see that. A named helper (`fn covers_whole_grain(in_grain, chunk_len, grain_bytes) -> bool`,
inverted) or even a named local would pay for itself.

Extracting the sparse-grain branch as `fn allocate_and_fill_grain(&self, gt_idx, gte_idx, gt_sector, in_grain, src) -> Result<()>`
would leave the loop body as three short arms.

### M8 — `GtCache.loaded_idx` uses a `usize::MAX` sentinel where `Option<usize>` says it in the type

- **Location:** `src/reader.rs:72-76` (definition), `:192`, `:301`, `:494`, `:516`
- **Category:** Opaque naming — a comment compensating for a type
- **Severity:** Medium
- **Test coverage:** Every read exercises the hit/miss path; the invalidation at `:494` is
  covered by `write_into_grain_with_unallocated_gt_allocates_table_too`.

```rust
struct GtCache {
    /// Index into `gd` of the table currently held; `usize::MAX` if empty.
    loaded_idx: usize,
    entries: Vec<u32>,
}
```

The doc comment is the tell: it exists purely to explain a magic value that
`Option<usize>` would make unnecessary. `cache.loaded_idx != gt_idx` (`:301`) then quietly
depends on `usize::MAX` never being a real GT index — true, but load-bearing and unstated.

`Option<usize>` makes `if cache.loaded != Some(gt_idx)` read correctly, makes the
invalidation `cache.loaded = None`, and deletes the comment. A further tidy: `entries` and
`loaded_idx` are always set and cleared together, so `Option<(usize, Vec<u32>)>` or a
`loaded: Option<LoadedGt>` makes the invariant structural.

### M9 — six header fields are parsed, stored, and never read, with no stated reason

- **Location:** `src/header.rs:34-45` (struct); confirmed unread across `src/` and `tests/`
- **Category:** Speculative code
- **Severity:** Medium
- **Test coverage:** Only `exposes_descriptor_and_directory_offsets` (`header.rs:199-206`)
  reads `over_head` and `compress_algorithm` back off the struct.

| Field | Read anywhere in `src/`? |
|---|---|
| `version` | no |
| `flags` | no |
| `rgd_offset` | no (module doc says "ignored here") |
| `over_head` | no |
| `unclean_shutdown` | no |
| `compress_algorithm` | checked as a **local** at `header.rs:76`; the **stored copy** is never read |
| `capacity`, `grain_size`, `descriptor_offset`, `descriptor_size`, `num_gtes_per_gt`, `gd_offset` | yes |

They are `pub` on a `pub` struct reachable via `VmdkReader::header()`, so they are API
surface and I would **not** propose deleting them. But six of twelve fields being
decorative, with no comment saying "exposed for callers, not used internally", makes a
reader hunt for uses that do not exist.

**One of these is a real gap worth a note.** VMDK's `flags` bit 16 also indicates
compressed grains. `compress_algorithm != 0` is rejected at `header.rs:76` but the flags
bit is not checked, so a `streamOptimized` image that signals compression only through
flags would slip past. Correctness-adjacent, same family as H4, worth its own ticket.

### M10 — unchecked `+` on the line immediately after a `checked_mul`

- **Location:** `src/reader.rs:137` and `src/reader.rs:164`
- **Category:** Inconsistent intent — the reader cannot tell if the omission is deliberate
- **Severity:** Medium
- **Test coverage:** The *bounds* rejection is covered
  (`descriptor_extending_past_eof_is_corrupt`, `grain_directory_past_eof_is_corrupt`); the
  **overflow** case is not.

```rust
let desc_byte_off = header.descriptor_offset.checked_mul(SECTOR_SIZE).ok_or(...)?;
let desc_byte_len = header.descriptor_size.checked_mul(SECTOR_SIZE).ok_or(...)?;
if desc_byte_off + desc_byte_len > dev_size {          // <- unchecked
    return Err(Error::Corrupt("descriptor extends past EOF"));
}
```

Two lines of scrupulous `checked_mul` followed by a bare `+` that can itself wrap. In
`dev` (`overflow-checks` on) this panics; in `release` (`Cargo.toml` sets `opt-level = 3`,
overflow checks off) it wraps to a small number, the EOF check *passes*, and the next line
does `vec![0u8; desc_byte_len as usize]` with a near-`u64::MAX` length — an allocation
abort rather than the clean `Corrupt` the code was clearly trying to produce. Same shape
at `:164` for the grain directory.

`checked_add` on both lines is a two-character-per-site fix and makes the intent uniform.

### M11 — the two allocators disagree about the u32 guard, and one guard is unreachable

- **Location:** `src/reader.rs:445-449` (`allocate_grain`) vs `src/reader.rs:463-466`
  (`allocate_grain_table`); the shared guarantee at `:433-437`
- **Category:** Duplicated-but-divergent logic + defensive code for an impossible case
- **Severity:** Medium
- **Test coverage:** Both allocators are covered for the success path; neither guard is
  reachable by a test.

`allocate_sectors` already guarantees `start + n <= u32::MAX` (`:433`), so its return value
is always in u32 range. Given that:

- `allocate_grain` (`:445-449`) casts with a bare `s as u32` — correct, relying silently on
  the guarantee.
- `allocate_grain_table` (`:463-466`) re-checks `if new_gt_sector_u64 > u32::MAX as u64`
  and returns `Error::Unsupported("grain table sector past u32 range")` — **dead code**;
  the branch cannot be taken.

Two adjacent functions calling the same allocator and disagreeing about whether its
postcondition can be trusted. A reader has to derive the postcondition themselves to know
which one is right. Fix: state the guarantee once (return `u32` from `allocate_sectors`, or
document the postcondition on it), then drop the unreachable branch and keep the cast.

### M12 — `SECTOR_SIZE` is private, so three test files redeclare it

- **Location:** `src/reader.rs:44` (`const SECTOR_SIZE: u64 = 512;` — module-private);
  `tests/synthetic.rs:26`, `tests/write.rs:24`, `tests/corruption.rs:25` (`const SECTOR: u64 = 512;`)
- **Category:** Magic number not centralised
- **Severity:** Medium
- **Test coverage:** N/A

The crate's most fundamental constant is defined four times under two different names,
and the three test copies can drift from the source copy without anything noticing.
`pub const SECTOR_SIZE: u64 = 512;` (in the `constants.rs`/layout block from M2) lets the
tests import it. Note `header.rs:29`'s `HEADER_SIZE: usize = 512` shares the *value* but not
the *meaning* — worth a comment saying the coincidence is not a dependency.

---

## Low severity

### L1 — `allocate_grain` uses single-letter locals `n` and `s`

- **Location:** `src/reader.rs:446-447` — **Category:** Misleading names — **Coverage:** covered by all write tests

```rust
let n = self.header.grain_size;
let s = self.allocate_sectors(n)?;
Ok(s as u32)
```

`n_sectors` / `start_sector`. Three-line function, so the cost is small, but this is the
only place in the crate that does it.

### L2 — the `dev_read`/`dev_write` wrappers are bypassed in `open_inner`

- **Location:** wrappers at `src/reader.rs:219-229`; bypassed at `:119`, `:141`, `:169`
- **Category:** Inconsistency — **Coverage:** covered

`open_inner` writes `dev.read_at(off, buf).map_err(fs_core_to_vmdk_error)?` three times
because it runs before `Self` exists; every other call site uses `self.dev_read(...)`.
Defensible, but a free-function `fn read_dev(dev: &dyn BlockDevice, off, buf) -> Result<()>`
that both paths use would remove the asymmetry and the three repeated `map_err`s.

### L3 — `read_at` has two identical `dst.fill(0)` branches nested three deep

- **Location:** `src/reader.rs:276-287` — **Category:** Deep nesting — **Coverage:** both
  branches covered (`whole_image_unallocated_reads_zero` and `unallocated_grain_reads_zero`)

```rust
if gt_sector == 0 {
    dst.fill(0);
} else {
    let grain_sector = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
    if grain_sector == 0 { dst.fill(0); } else { ... }
}
```

Both arms mean "no backing storage → zeros". Resolving to a single
`let grain_sector = if gt_sector == 0 { 0 } else { self.lookup_grain(...)? };` followed by
one `if grain_sector == 0` flattens it and makes the shared meaning explicit.

### L4 — module doc says "we only care about three things", and the third is a non-thing

- **Location:** `src/descriptor.rs:4-10` — **Category:** Comment that misleads — **Coverage:** N/A

The three bullets are: `createType` (cared about), the extent line (allegedly cared about
— see H3, it is not), and "`ddb.*` lines (geometry, etc.) are ignored". Ignoring something
is not caring about it. After H3 is resolved the honest count is one.

### L5 — `parse_kv` is parameterised over `key` but only ever called with one

- **Location:** `src/descriptor.rs:84-91`, called once at `:43` — **Category:** Speculative
  generality — **Coverage:** covered indirectly

Borderline *acceptable pattern* — the generality costs nothing and is the obvious shape if
`parentFileNameHint` gets parsed (H4). Listing it only so it is not re-discovered later.
If H4 is taken, this becomes justified and should be left alone.

### L6 — `HEADER_SIZE: usize` among u64 sizes forces casts at the boundary

- **Location:** `src/header.rs:29`, cast at `src/reader.rs:113` — **Category:** Misleading
  type — **Coverage:** covered by `rejects_header_shorter_than_512_bytes`

`usize` is right for `[0u8; HEADER_SIZE]` and wrong for `dev_size < HEADER_SIZE as u64`.
Minor; a `HEADER_SIZE_BYTES: u64` alias or a single documented cast site would do.

---

## What to fix first

Ordered by value per unit of risk. Every item in tiers 1–3 sits under existing test
coverage, so a dev-loop run can move through them with the 37-test baseline as the guard.

**Tier 1 — the theme, and the cheapest structural wins.** Do these together; they are one
idea.

1. **H1** — `CreateType` enum. Fully covered by existing descriptor tests, no behaviour
   change, and it is the finding the rest of the naming problems hang off. Start here.
2. **M1** — `ExtentAccess` / `ExtentKind` enums, immediately after H1 while the file is
   open. Add the missing `kind` test.
3. **H3 option (b)** — fix the lying comment in `descriptor.rs` and name the `_descriptor`
   binding for what it is. Pure documentation, zero risk. Defer option (a) — validating
   extent sectors against capacity is a behaviour change and needs a qemu-validation run.

**Tier 2 — constants, once, in one place.** All mechanical, all covered.

4. **M2 + M12 + M3** — one pass. Create the layout-constant home (`GTE_SIZE`,
   `pub SECTOR_SIZE`, the `HEADER_OFF_*` family), then replace the seven `4`s, export
   `SECTOR_SIZE`, and move `corruption.rs`'s three already-named offsets into it. This is
   the skill's centralisation rule applied to a family that is currently scattered across
   five files.

**Tier 3 — the duplicated core.** Highest single readability payoff, but the largest diff.

5. **H2** — extract the grain-walk. Do it *after* tier 2 so the extracted code already uses
   named constants. The write tests are strong enough to catch any address-math slip
   immediately.
6. **M6, M7** — split `open_inner` and `write_at`'s chunk body. Natural follow-on once H2
   has created the vocabulary.
7. **M8, M11, L1, L3** — small named-thing cleanups; sweep them up in the same pass.

**Tier 4 — test hygiene.** Independent of everything above; can be done in parallel or
first if you want a quick win.

8. **M4 + M5** — `tests/common/mod.rs`. Deduplicates four `build_header`s, three descriptor
   builders, four `tmp_path`s, and fixes the fixture leak in `synthetic.rs`/`write.rs` by
   giving all four suites the `TempPath` that two of them already have. **No test is
   deleted** — the fixture builders move, the `#[test]` functions stay exactly as they are.

**Separate track — not refactors, do not fold into a readability PR.** These three are
behaviour changes surfaced by the review and each deserves its own commit, its own test,
and (for H4) a qemu-validation run:

- **H4** — reject delta/linked-clone disks instead of silently reading them as
  half-zeroed. This is the most consequential thing in this report.
- **M9 note** — check `flags` bit 16 alongside `compress_algorithm`.
- **M10** — `checked_add` at `reader.rs:137` and `:164`.

---

## Test Results

No code changed, so before and after are identical. Recorded as the baseline contract for
a future Phase 2.

| | Before | After |
|---|---|---|
| Tests passing | 37 | 37 (unchanged — no code modified) |
| Tests failing | 0 | 0 |
| Tests added | — | 0 |
| Feature-gated tests not run | 6 (`qemu-validation`) | 6 |
| `cargo clippy --all-targets` | clean, 0 warnings | clean, 0 warnings |
| Coverage tooling | not run (no `cargo-llvm-cov`/`tarpaulin` configured in this repo) | — |

Coverage in the findings above is stated per-item from reading the test suite against the
code, not from an instrumented run. If you want hard numbers before a refactor, adding
`cargo-llvm-cov` to the chore file would be the way — but the qualitative picture is
strong: every High and Medium finding except H4, M1(`kind`), M9 and M10 sits under
existing tests.

## Gaps worth knowing about

Not smells, so not counted in the 22 — but they came up while reading and affect how
confidently a refactor can proceed:

- **No test opens a real VMware-produced VMDK.** All fixtures are hand-built here or
  qemu-built behind a feature gate. The hand-built fixtures encode this crate's own
  understanding of the layout, so a shared misreading would be invisible.
- **`chores.yml:31-33` flags its own hazard** and it is still live: `include/vmdk.h`
  `#include`s `fs_core.h`, which the chore copies from `../rust-fs-core/include/`, and
  nothing checks that path agrees with the one `Cargo.toml` resolves `am-fs-core` from.
  The file says so in capitals. Out of scope for this pass, but it is a real
  build-correctness trap.
- **`tests/write.rs:406-409`** contains a comment with a visible arithmetic false start
  ("= 524.something — wait, recompute:"). Harmless and the final answer is right, but it
  is thinking-out-loud left in the tree.
