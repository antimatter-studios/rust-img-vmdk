# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work and records everything as open; this is the current position.
Updated 2026-08-30.

**22 findings** — 4 High, 12 Medium, 6 Low. This covers the 16 High and Medium.

| | High | Medium |
|---|---|---|
| Fixed | 1 | 4 |
| Left for a human decision | 3 | 4 |
| Fixable, not yet done | 0 | 4 |

---

## High

### H1 — `createType` dispatch is a nine-arm identity `match` — **needs your decision**

Accurate: nine arms that map a string to itself. Replacing it with a type is
right, and it changes how the descriptor's parsed form is represented — a design
choice about the crate's internal model.

### H2 — `read_at` and `write_at` carried two copies of the grain-address walk — **fixed**

Both computed `in_grain`, `grain_idx`, `gt_idx`, `gte_idx` and `chunk_len` the
same way, and both bounds-checked the grain directory the same way — then
diverged, correctly: the reader zero-fills an unallocated grain, the writer
allocates one.

`grain_address` and `gd_entry` carry the shared half; the divergence stays where
it belongs. Two copies of an index calculation is how one of them ends up off by
a level, and this one has two levels to be off by.

Both walks were covered before — swapping `gt_idx` and `gte_idx` in either failed
2 tests — which is what made consolidating them safe rather than merely tidy.

### M4, M5 — triplicated fixture builders; two test files leak fixtures on panic — **fixed**

**M5 first, because it is the one with a consequence.** `corruption.rs` and
`qemu_validation.rs` each had a `TempPath` with a `Drop`. `synthetic.rs` and
`write.rs` returned a bare `PathBuf` and removed it at the end of the happy path
— so **any assertion that panicked left a `.vmdk` behind**, and the failure path
is exactly when a fixture is most likely to be abandoned and least likely to be
noticed, because attention is on the failure.

`tests/common/mod.rs` holds one self-deleting `TempPath` and the portable
`WriteAt`. The proof is a test rather than an assertion about intent: it runs a
panicking closure under `catch_unwind` and then requires the temp directory to
hold nothing named for it. Disabling the `Drop` fails it.

M4's builders now share that harness and take `&Path` rather than `&PathBuf`, so
a fixture type can change without touching them.

### H3 — the parsed descriptor is discarded, and the doc says otherwise — **fixed earlier**

[#23](https://github.com/antimatter-studios/rust-img-vmdk/pull/23) — VMDK images
declaring a parent are now refused rather than read as though standalone.

### H4 — delta / linked-clone disks are indistinguishable from full disks — **needs your decision**

The same area #23 addressed, and the remaining half is a feature question:
whether this crate should *support* delta disks, or keep refusing them. Not a
defect to correct.

---

## Medium

### M2 — the GD/GT entry size `4` is an unnamed literal at six sites — **fixed**

`GD_GT_ENTRY_SIZE`. Both tables are arrays of little-endian `u32` sector
numbers, so the same 4 governs a GD's length, a GT's length, and the offset of
any entry in either — and as a bare literal it is indistinguishable from an
unrelated 4.

### M8 — `GtCache.loaded_idx` uses a `usize::MAX` sentinel — **fixed**

`Option<usize>`. The sentinel was something every reader of the field had to
know about and no type enforced; four sites carried it.

### M10 — unchecked `+` immediately after a `checked_mul` — **fixed**

Two sites. Both operands are attacker-supplied sector counts scaled to bytes, so
**the sum can overflow even when neither product did** — and an overflowing sum
wraps to a small number that passes the EOF test it was written to enforce. Now
`checked_add`, like the multiplications above them.

### M12 — `SECTOR_SIZE` was private, so three test files redeclared it — **fixed**

Now `pub`, with a note saying why.

### M1 — `Extent.access` and `Extent.kind` are bare `String` over closed sets — **needs your decision**

Same family as H1: correct, and it changes the parsed model.

### M11 — the two allocators disagree about the u32 guard — **needs your decision**

`allocate_sectors` already guarantees the bound, so one guard is unreachable and
the two allocators disagree about whether to check. Which one is right depends
on whether the guarantee should be relied on or restated — a decision about
where the invariant lives.

### M9 — six header fields parsed, stored, never read — **needs your decision**

Whether to use them or drop them depends on what this crate intends to support.

### M6, M7 — `open_inner` at 87 lines; `write_at`'s four-level nesting — **needs your decision**

Both are restructures of paths that establish or depend on invariants, with no
behavioural test covering the seams.

### M3 — header field offsets were bare numerals — **fixed**

Thirteen offsets written as literals in `SparseHeader::parse`, decodable only
against the ASCII table in the module doc — which is good documentation and is
not the same as a name. A literal in a parse expression carries no way to tell a
correct offset from a typo; a name can be checked against the table by eye.

**`compressAlgorithm` at 77 is the one that matters.** It is where the
compression flag lives, so reading it from the wrong place means silently
accepting a `streamOptimized` image as an ordinary one, and then decoding its
grains as raw data. Its doc now says why 77 looks wrong and is not: four
single-byte end-of-line fields sit at 73..=76, so the word is unaligned by the
format's own doing.

**The need had already been felt and half-met.** `tests/corruption.rs` named
three of these itself — in a test file, invisible to the parser that also needed
them. They now come from the parser's table, so a test that corrupts "the
descriptor offset" corrupts whatever the parser reads as the descriptor offset.
If the two ever disagreed, the test would pass while corrupting a neighbouring
field.

The module is `pub`, not `pub(crate)` as the report suggested — integration tests
compile as a separate crate, so `pub(crate)` cannot reach them, and the two
halves of that suggestion contradict each other. Public is the right call anyway
for a format crate: the layout is already published as a drawing in this module's
documentation, so naming the offsets commits to nothing the drawing did not.

Mutation-checked: `COMPRESS_ALGORITHM` 77→78, `GD_OFFSET` 56→48 and
`DESCRIPTOR_SIZE` 36→44 each fail 2 tests.

### M4, M5 — triplicated fixture builders; two test files leak fixtures on panic — **fixable, not yet done**

Both genuine, and M5 is worth doing — a panicking assertion leaves a temp file
behind. They are one change together, in `tests/common/mod.rs`.

---

## Verification

43 tests pass across all binaries, up from 42. `chore lint` clean.
Nothing here changes behaviour except M10, which refuses an input that
previously wrapped past a bounds check.
