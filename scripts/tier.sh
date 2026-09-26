#!/usr/bin/env bash
# tier.sh LABEL LOG-NAME MAX-LINES MAX-BYTES -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<LOG-NAME>.log; a pass prints one verdict line naming the log, a
# failure prints one line naming the log and the command's own status, and a
# run that passed but printed more than its budget fails with status 65.
#
# ONE WRAPPER FOR BOTH CALLERS. chores.yml runs the tiers for a person at a
# terminal and .github/workflows/ci.yml runs them for the gate, and they run
# the SAME command through the SAME budget -- so a tier that has outgrown its
# budget says so here, before the push, rather than in a CI log nobody was
# going to read. tests/ci_profile.rs checks that the two files agree on every
# tier's numbers; the duplication is deliberate (the workflow cannot read
# chores.yml without installing chore on three runner platforms) and it is
# checked rather than trusted.
#
# WHY THE BUDGET IS PART OF THE TASK. A passing run that prints three
# thousand lines hides the twenty that matter, and every reader pays for it:
# a person scrolling, a CI log viewer, and an agent working in the
# repository, which re-reads its whole transcript on each step and so pays
# for one verbose run many times over. Measured across this constellation:
# 4,661M cache-read tokens against 9.5M of output, and command output was the
# largest single contributor a repository controls.
#
# The budgets themselves are in chores.yml, next to the command each one
# bounds, and every one of them was MEASURED -- see the table there. Raise
# one deliberately when a tier grows, the way the executed-test floors are
# raised; a budget nobody can breach measures nothing.
#
# VERBOSE. `OUTPUT_BUDGET_VERBOSE=1`, or `--verbose`/`-v` in the chore
# invocation's CLI_ARGS (`chore test:debug -- --verbose`), streams the run as
# it happens as well as logging it. It does NOT lift the budget: the log is
# the same size either way, and a tier that has outgrown its budget should
# say so whether or not anybody was watching.
#
# THE VARIABLE WAS `FLTH_VERBOSE` until the wrapper moved to rust-fs-core,
# and the same rename happened to `FLTH_FAIL_TAIL`, now
# `OUTPUT_BUDGET_FAIL_TAIL`. If you are here because `--verbose` stopped
# working, that is why: the canonical script does not read the old names, so
# setting one does not error -- the run simply stays quiet. It says so on
# stderr rather than ignoring it, which is the only warning you will get.
#
# A FAILING TIER NO LONGER PRINTS A TAIL BY DEFAULT. rust-fs-core#164 made
# the tail opt-in (`OUTPUT_BUDGET_FAIL_TAIL=40`, or `--tail N`) because the
# reader who pays most for it -- an agent re-reading its transcript on every
# later step -- is charged for those lines many times over, and they are
# rarely the lines it needs. The failure line names the log; CI uploads
# tmp/logs/ as an artefact and locally it is already on disk.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# THE WRAPPER BELONGS TO rust-fs-core, AND THIS REPOSITORY DOES NOT KEEP A
# COPY OF IT.
#
# scripts/output-budget.sh used to be vendored here, from
# fs-linux-test-harness v0.1.0. A committed copy is a copy that drifts: the
# family had several, each internally consistent, each reached a different
# way, and nothing comparing them. The canonical one now lives in
# rust-fs-core and is resolved AT RUNTIME (rust-fs-core#153).
#
# WHERE IT IS LOOKED FOR, in order, and a miss at any resolved location is
# fatal rather than a reason to try the next one:
#
#   1. $FS_CORE_ROOT -- an explicit answer, absolute or relative to this
#      repository. It is what ci.yml sets, and what tests/output_budget.rs
#      points at a scratch directory to prove the refusals below are real.
#      Set but unusable is a configuration mistake, so it is FATAL: falling
#      through would silently run the tiers against a different core than
#      the one somebody asked for.
#   2. ../rust-fs-core -- the sibling checkout. FIRST AMONG THE TWO
#      DISCOVERED SOURCES, because a coordinated local change to the wrapper
#      should be exercised here on the next run, and because THIS SUITE RUNS
#      ON windows-latest: under Git Bash a `C:\...` path out of `cargo
#      metadata` is not a path that can be tested or copied, while the
#      sibling path is POSIX on every runner.
#   3. whatever `cargo metadata` says the am-fs-core package root is -- the
#      answer for a standalone checkout that takes core from the registry
#      rather than from a sibling.
#
# WHAT IS VERIFIED IS THE API STRING, NOT A DIGEST. `--version` must print
# exactly "rust-fs-core-output-budget 1". rust-fs-ntfs also pins a SHA-256 of
# the file; this repository deliberately does not. A digest repeated in seven
# repositories has to be updated in seven repositories for any change to the
# wrapper, however small -- which is exactly the lockstep this migration
# exists to remove. The version string is the contract: it moves when the
# BEHAVIOUR moves, and not when a comment does.
CORE_SCRIPT_REL="scripts/output-budget.sh"
CORE_API="rust-fs-core-output-budget 1"
CORE_MIN_VERSION="v0.2.13"
CORE_SIBLING="$REPO/../rust-fs-core"

die() {
    echo "tier.sh: $1" >&2
    shift
    for line in "$@"; do
        echo "         $line" >&2
    done
    exit 1
}

# The wrapper must speak the API this script calls it with. A present but
# wrong copy is a failure, never a reason to look somewhere else.
verify() {
    local path="$1" source="$2" version
    version="$(bash "$path" --version 2>/dev/null || true)"
    [ "$version" = "$CORE_API" ] || die \
        "the output-budget wrapper found via $source does not speak this API." \
        "$path" \
        "--version said: ${version:-<nothing>}" \
        "expected:       $CORE_API" \
        "That is rust-fs-core $CORE_MIN_VERSION or later. This is fatal rather" \
        "than a reason to look elsewhere: a wrapper that answers differently" \
        "is not the one the tiers were measured against."
}

if [ -n "${FS_CORE_ROOT:-}" ]; then
    # Relative is resolved against the repository, not the caller's working
    # directory, so the value in ci.yml means the same thing from any step.
    case "$FS_CORE_ROOT" in
        /*) core_root="$FS_CORE_ROOT" ;;
        *)  core_root="$REPO/$FS_CORE_ROOT" ;;
    esac
    [ -f "$core_root/$CORE_SCRIPT_REL" ] || die \
        "FS_CORE_ROOT is set and holds no $CORE_SCRIPT_REL." \
        "FS_CORE_ROOT=$FS_CORE_ROOT -> $core_root" \
        "Point it at a rust-fs-core checkout at $CORE_MIN_VERSION or later," \
        "or unset it to fall back to ../rust-fs-core."
    CORE_SCRIPT="$core_root/$CORE_SCRIPT_REL"
    verify "$CORE_SCRIPT" "FS_CORE_ROOT"
elif [ -f "$CORE_SIBLING/$CORE_SCRIPT_REL" ]; then
    CORE_SCRIPT="$CORE_SIBLING/$CORE_SCRIPT_REL"
    verify "$CORE_SCRIPT" "../rust-fs-core"
else
    # THE REGISTRY ANSWER, and the only branch that needs a JSON parser.
    # python3 is on all three runners; if it is not here, say so rather than
    # printing a shell error about a missing command.
    command -v python3 >/dev/null 2>&1 || die \
        "../rust-fs-core has no $CORE_SCRIPT_REL and python3 is not available" \
        "to read \`cargo metadata\`, which is the only other place the" \
        "wrapper can be found. Install python3, or check out rust-fs-core" \
        "$CORE_MIN_VERSION or later beside this repository."
    core_package_root="$(cargo metadata --format-version 1 --locked \
        --manifest-path "$REPO/Cargo.toml" 2>/dev/null | python3 -c '
import json, sys

try:
    packages = json.load(sys.stdin)["packages"]
except Exception:
    sys.exit(0)
print(next((p["manifest_path"].rsplit("/", 1)[0]
            for p in packages if p["name"] == "am-fs-core"), ""))
')"
    [ -n "$core_package_root" ] && [ -f "$core_package_root/$CORE_SCRIPT_REL" ] || die \
        "no rust-fs-core supplied $CORE_SCRIPT_REL." \
        "Looked at: \$FS_CORE_ROOT (unset), $CORE_SIBLING/$CORE_SCRIPT_REL," \
        "and the am-fs-core package \`cargo metadata\` resolves." \
        "The wrapper is rust-fs-core's and is NOT vendored here. It ships" \
        "from $CORE_MIN_VERSION onwards and must answer \`--version\` with" \
        "\"$CORE_API\"." \
        "Check out rust-fs-core at $CORE_MIN_VERSION or later beside this" \
        "repository, or set FS_CORE_ROOT to one that is."
    CORE_SCRIPT="$core_package_root/$CORE_SCRIPT_REL"
    verify "$CORE_SCRIPT" "cargo metadata"
fi

# A COPY FOR THE RUN, DELETED WHEN IT ENDS. The source is somebody else's
# checkout and may be a tag, a branch or a read-only registry directory; the
# run should not depend on it staying still, and nothing should be left
# behind that a later reader could mistake for a vendored copy. tmp/ is
# gitignored and already holds the tier logs.
BUDGET="$REPO/tmp/output-budget.$$.sh"
mkdir -p "$REPO/tmp"
cp "$CORE_SCRIPT" "$BUDGET"
trap 'rm -f "$BUDGET"' EXIT

[ $# -ge 5 ] || { echo "tier.sh: usage: tier.sh LABEL LOG MAX-LINES MAX-BYTES -- CMD..." >&2; exit 2; }
LABEL="$1"; LOG_NAME="$2"; MAX_LINES="$3"; MAX_BYTES="$4"; shift 4
[ "${1:-}" = "--" ] && shift
[ $# -gt 0 ] || { echo "tier.sh: no command" >&2; exit 2; }

# `chore test:debug -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# OUTPUT_BUDGET_VERBOSE itself, so mapping the flag onto it is all that is
# needed -- and it means the environment variable and the flag cannot
# disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export OUTPUT_BUDGET_VERBOSE=1 ;;
esac

# `bash "$BUDGET"` rather than exec'ing it directly: a Windows checkout
# arrives without the executable bit, and this suite runs on windows-latest.
# Not `exec`, either -- the trap above has to run when it returns.
bash "$BUDGET" \
    --log "$REPO/tmp/logs/$LOG_NAME.log" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$LABEL" \
    -- "$@"
