# Working in rust-img-vmdk (agent guide)

Pure-Rust VMDK reader and writer, including stream-optimized images, validated against `qemu-img`. This file is the fast path
for an agent picking up work here, so the workflow does not have to be
re-derived each time. It points at the existing docs rather than duplicating
them:

- **README** → what the crate does, how it is built, and what does not work yet.
- **`chores.yml`** → every task named below, and what each one actually runs.
- **`.github-guard`** → what must pass before `main` takes a merge.

The section between the BEGIN/END markers below is **shared, byte-identical,
with every repository in this family**. Do not edit it here: change the
canonical copy and propagate it, or `scripts/agents-core-check.sh` will fail.
Everything after the END marker is specific to this repository.

<!-- BEGIN SHARED BLOCK: agent-core v2 sha256:38af4d2c5377d38ab382baa4eab4aa679841e2b4eba4f4d01dacd255ffa7d32e -->
## Claiming work

Several agents work these repositories at the same time. Before you start on
an issue, claim it, so nobody else spends a session on what you are already
doing. The lock is a **GitHub label**, because labels are shared state that
every agent can read and change without posting comments into the thread.

**Before starting.** Check, claim, then read back:

```sh
gh issue view <N> --json labels                      # holds `claimed`? pick another
gh issue edit <N> --add-label claimed --add-label claim/<session>
gh issue view <N> --json labels                      # read back and confirm
```

`<session>` is your session name — `agent-<random4>-<isodate>`, e.g.
`agent-3f7c-2026-09-22`. Create the `claim/<session>` label if it does not
exist.

**Resolving a race.** Adding a label is not compare-and-swap: two agents can
both add `claimed` and both believe they won. That is what the read-back is
for. If it shows more than one `claim/*` label, the **lexically lowest**
session keeps the issue; every other agent removes its own `claim/*` label and
picks different work. Each racer computes the same answer independently, so no
further coordination is needed.

**When you finish or stop.** Remove both labels — on merge, or the moment you
abandon the work:

```sh
gh issue edit <N> --remove-label claimed --remove-label claim/<session>
```

Delete your `claim/<session>` label from the repository at the end of your
session so they do not accumulate.

**Reclaiming a stale claim.** An agent that dies holding a claim would block an
issue forever. If `claimed` was applied more than 12 hours ago and the holder's
branch has no commits since, any agent may take it: remove the stale `claim/*`,
add your own, and say so in the issue.

**This is a convention, not a fence.** Nothing enforces it. An agent that
ignores it duplicates work; it cannot corrupt anything. Honour it anyway.

## Work in a worktree

Every working copy is a **git worktree** of an existing checkout, made with
`git worktree add`. Never `git clone` a second, unlinked copy — not for a
branch, a PR, a review, or a sibling you need at another ref:

```sh
git -C <checkout> fetch origin
git -C <checkout> worktree add <path> -b <type>/<name> origin/main   # new work
git -C <checkout> worktree add --detach <path> <tag>                 # a sibling at a pinned ref
git -C <checkout> worktree remove <path>                             # when done
```

A worktree shares the checkout's objects and remotes, and `git worktree list`
shows it to every agent on the machine, so nobody else mistakes it for
abandoned work or loses track of it. An unlinked clone copies all the history
again, is invisible to that list, and gets left behind in `/tmp` long after the
work that made it is merged. Remove your worktree when you finish.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening pull requests.

Each repository names any further skills of its own below.

## A bug fix starts with a red

**Prove it is broken first** — a failing check or test — *then* fix it, *then*
prove that same check is green, *then* confirm the full baseline still passes.
Never write the fix before you have a red. A fix with no failing test to its
name is a claim, not a result.

## Nothing skips

A test that cannot run **fails**, naming the task that would provide what it
needed. Never add an early return for a missing fixture, tool or VM: a skipped
test reads exactly like a passing one, and a suite that quietly declines to run
is indistinguishable from a suite that passes.

Where a tier reports skips or ignored tests, that is a gate, not a note.

## Validate against something that is not us

A driver's own readers share its interpretation of the format, so they cannot
catch a misreading: the mistake is baked into the fixture *and* the parser, and
they agree with each other while disagreeing with every real filesystem. Unit
tests over self-built fixtures prove self-consistency, not correctness.

Every structure that is parsed or written gets a cross-validation test against
an **independent oracle** — the platform's own tools, a real kernel, or a third
implementation — before it is considered done. Each repository names its
oracles below.

## Output is budgeted

Test tiers run through `scripts/tier.sh`, which runs the suite **quietly**: the
whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line naming
that log, and a failure prints the verdict, the command's status and the log's
path — `--tail N`, or `OUTPUT_BUDGET_FAIL_TAIL=N`, prints the tail for whoever
is watching. **Read the log**: a failing tier names it and does not recite it.
CI keeps the logs as an artifact, so the detail is always retrievable.

The budget caps the log, not merely what is shown, and every number in the
table was measured. A run that passes but prints more than its budget **fails**.

The reader who pays most for a noisy suite is an agent that re-reads its whole
transcript on every step, and so pays for one loud run many times over. If a
tier legitimately grows, raise its row **with the measurement that justifies
it**. Do not silence output to fit, and do not route around `tier.sh`.

## Commits and branches

- Branches are `<type>/<name>`, matching the commit type: `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- A commit is a subject plus flat one-sentence bullets. Subjects are
  declarative, not imperative: "the run-end bound is checked", not "check the
  run-end bound".
- **No AI attribution and no co-author trailers**, in commits or in pull
  request descriptions.
- `main` takes **squash merges only**.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache).
  Shelling out to a copyleft CLI as a *test oracle* is fine — linking or
  copying it is not.
- **Each of these is a standalone project.** Never mention a consuming
  application in the README, the source, or CLI help.
<!-- END SHARED BLOCK: agent-core v2 -->
## What this is

Pure-Rust VMDK reader and writer over `rust-fs-core`, including stream-optimized
images, linked into the app as a staticlib.

## Running tests

```sh
chore test          # the suite
chore testqemu      # against qemu-img
chore testrelease   # release profile
chore lint          # fmt, the agent-core check, clippy
chore staticlib     # what the app links
```

CI runs `test`, `test-release`, `qemu-validation`, `fmt`, aggregated by `ci-ok`.

## The oracle is qemu-img

An image this crate writes must be one `qemu-img` reads identically, and one
`qemu-img` wrote must read identically here. If `qemu-img` is missing the job
**fails**; it does not skip.

## Grains must not overlap the metadata

`src/reader.rs` keeps a sorted, merged range set of the fixed metadata — header,
descriptor, live directories — plus every grain table a live directory names or
this session allocates. A grain that overlaps any of it is corruption, and
grain-table pointers are checked against that set and against EOF **before**
anything is published into them. The guard used to be a single scalar floor,
which a trailing `rgd_offset` could lift over every grain (#63).

## The changelog has a format guard

`tests/changelog.rs` fails on duplicate `### Added` headings under one version.
Add your entry under the heading that is already there.

## How this format allocates, and the one place it asks for room

Appending is the only way a monolithicSparse VMDK allocates: put the grain, or
the grain table, at the file's tail and then record where it went. That used to
work because a write past the end of a `FileDevice` grew the file underneath
it — and rust-fs-core#75 made it a refusal, correctly: `size_bytes()` reported
the construction-time length while the file grew, so `CachingDevice` could
serve bytes no cached read could reach (rust-fs-core#70). The `rust-fs-core` pin
sat at `v0.2.10` for six releases because of it, with 12 failing write tests
waiting behind the bump — the largest count of the four image crates.

`BlockDevice::set_len` (rust-fs-core#161) is the replacement, and there is
**exactly one call**, in `VmdkReader::allocate_sectors`. Three writes land at
the tail — the full-grain zero-init and the payload in `allocate_and_write`,
and the fresh table in `allocate_blank_grain_table` — and all three are reached
through that function, which is the crate's declared allocation cursor. Adding
it at the write sites instead would be three calls saying the same thing.

It is called **under the cursor lock and before the cursor moves**. The lock is
what serialises allocation, so extending inside it means two allocators cannot
be handed the same tail; asking before the cursor moves means a refusal leaves
the cursor describing the device that is really there.

**`file_extent()` is `dev.size_bytes()` again.** It used to be
`size_bytes().max(alloc_cursor * SECTOR_SIZE)`, because the device's declared
length stopped being the file's the first time anything was appended — its own
doc comment said so. `set_len` moves the declared length with the file, so the
`max()` became a number compared against itself. Two answers to one question is
how they come to differ.

`VmdkReader`'s own `impl BlockDevice` answers `can_grow() == false`
explicitly. The image file grows constantly; the **guest disk** does not — its
length is the header's `capacity`, so changing it means rewriting the header and
the grain directory sized from it, not writing past the end.

Both trait methods are **defaulted** to a refusal, so a wrapping device that
does not forward them turns a growable device into one that cannot allocate.
`StallingDevice` and `CountingFlushes` in `tests/write.rs` forward both.

## The output budget comes from rust-fs-core

Every tier runs through rust-fs-core's `scripts/tier.sh`, **run in place**
from the `../rust-fs-core` checkout at the version this repository pins:
`bash ../rust-fs-core/scripts/tier.sh LABEL LOG LINES BYTES -- COMMAND`.
There is no copy of it, of `scripts/output-budget.sh`, or of any other family
script in this repository, and rust-fs-core's `family-check` (run in CI)
refuses one. Bumping the pinned core version is how the scripts are upgraded;
nothing is recopied. See rust-fs-core#153 and #212.

**`OUTPUT_BUDGET_VERBOSE`, not `FLTH_VERBOSE`.** The canonical script does
not read the old name, and setting it does nothing at all.

## What gates a merge

One required check, `ci-ok`, declared in `.github-guard` and aggregating every
job in `ci.yml`. `fuzz.yml` (nightly cron plus dispatch) and `release.yml`
(tag-driven) never report on a pull request and must never be required.

`chore check:ci-gate` holds both halves of that mechanically — every job in
`ci.yml` must appear in `ci-ok`'s `needs:`, and `.github-guard` must require
`ci-ok` and nothing else. The task runs `scripts/core.sh ci-gate` and nothing else,
so the script is what can be tested, reviewed and run without `chore` at all.
It replaced `tests/ci_aggregate_gate.rs`: that parsed a YAML file and compared
strings, exercising nothing this crate ships, and as a `cargo test` it counted
towards the executed-test floor the gate itself enforces.

Judging mergeability from check **conclusions** is unreliable: an in-progress
`CheckRun` reports its conclusion as an empty string, and a `StatusContext` has
no conclusion field at all. Read `mergeStateStatus` and
`statusCheckRollup.state`.

## Never grow a shared tool to solve a problem here

**Never grow a shared tool to solve a problem in this repository.** `chore` is
a general-purpose task runner this project merely consumes; the same goes for
`github-guard` and the agent-skills hooks. If something needed here looks like
it belongs inside one of them, it does not. Solve it here, or ask first. The
tell is a release: if a shared tool needs a new version cut whose only purpose
is to unblock this project, the code is in the wrong repository.
