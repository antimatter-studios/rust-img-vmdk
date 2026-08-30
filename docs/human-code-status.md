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

### H2 — `read_at` and `write_at` carry two copies of the grain-address walk — **fixable, not yet done**

Genuine duplication. Deferred as one deduplication change with the synthetic
tests as the contract, rather than folded into a pass that is mostly names.

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

### M3, M4, M5 — header offsets duplicated four ways; triplicated fixture builders; two test files leak fixtures on panic — **fixable, not yet done**

All genuine. M5 in particular is worth doing — a panicking assertion leaves a
temp file behind — but all three are the same kind of mechanical change and
belong together.

---

## Verification

42 tests pass across all binaries, unchanged in number. `chore lint` clean.
Nothing here changes behaviour except M10, which refuses an input that
previously wrapped past a bounds check.
