//! `scripts/tier.sh` resolves the output-budget wrapper, or refuses to run.
//!
//! # WHAT CHANGED, AND WHAT THIS FILE GUARDS NOW
//!
//! This repository used to carry `scripts/output-budget.sh`, a vendored copy
//! of fs-linux-test-harness's, and this file pinned that copy's behaviour --
//! exit 65 for a breached budget, the command's own status for a failure,
//! silence on success. The copy is gone (rust-fs-core#153). The canonical
//! wrapper belongs to `rust-fs-core` and `scripts/tier.sh` resolves it at
//! runtime, so those promises are now rust-fs-core's to keep and its own
//! suite's to check. Re-asserting them here would be this repository testing
//! another repository's file, from the wrong side of the boundary, and
//! failing for changes it has no say in.
//!
//! What IS this repository's to keep is the RESOLVER, and that is what these
//! tests hold:
//!
//! 1. a tier runs its command through the wrapper it resolved -- a passing
//!    tier is quiet and names its log, a failing tier hands on the COMMAND's
//!    status, and a breached budget exits 65 so it is not mistaken for a red
//!    suite;
//! 2. `CLI_ARGS=--verbose` streams the run, which is the only mechanical
//!    check that `tier.sh` exports `OUTPUT_BUDGET_VERBOSE` and not the
//!    `FLTH_VERBOSE` it used to -- that rename FAILS SILENTLY, since the
//!    canonical script simply does not read the old name;
//! 3. the resolver REFUSES a core that is absent, and REFUSES one whose
//!    `--version` does not speak the API this repository calls it with. A
//!    resolver that quietly fell back on either would run the tiers against
//!    something nobody chose, which is the drift the migration removed.
//!
//! # WHY NOT PIN THE BYTES
//!
//! rust-fs-ntfs pins a SHA-256 of the wrapper. This repository pins the
//! `--version` string instead, on purpose: a digest repeated across the
//! family has to be updated everywhere for any edit to the wrapper, however
//! small, which recreates exactly the lockstep this migration exists to
//! remove. The API string moves when the behaviour moves and not when a
//! comment does, so it is the contract worth checking.
//!
//! # NOTHING HERE SKIPS
//!
//! Every tier in this repository goes through `scripts/tier.sh`, so a host
//! that cannot run it cannot run the suite. There is no arrangement of
//! missing `bash`, missing `rust-fs-core` or missing wrapper under which
//! these tests pass quietly: they fail and name what would provide the
//! thing they needed.
//!
//! The other half of the arrangement -- that every tier in `ci.yml` and
//! `chores.yml` actually GOES through `tier.sh`, under a non-zero budget,
//! with the two files agreeing on the numbers -- is in `tests/ci_profile.rs`,
//! which is the file that already parses both.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

/// One tier at a time, because `tier.sh` names its copy of the wrapper after
/// its own process id and `the_copy_of_the_wrapper_does_not_outlive_the_run`
/// looks for copies by pattern. Two tiers in flight at once would let one
/// test see the other's live copy and call it a leak. Tests in a binary run
/// on threads; cargo runs the binaries one at a time, and this is the only
/// one that runs `tier.sh`.
static ONE_TIER_AT_A_TIME: Mutex<()> = Mutex::new(());

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The `bash` that can actually run a shell script.
///
/// # `bash` ON PATH IS NOT BASH ON A WINDOWS RUNNER
///
/// `C:\Windows\System32\bash.exe` is the WSL launcher, it ships with the
/// operating system, and `System32` comes early in `PATH` -- so
/// `Command::new("bash")` finds it before Git Bash. With no WSL
/// distribution installed it prints nothing useful and exits 1, which is
/// how every test in this file failed on `windows-latest` while the same
/// scripts ran perfectly in the workflow: an Actions step that says
/// `shell: bash` is handed Git Bash by name and never consults `PATH`.
///
/// So this asks for Git Bash by name on Windows and falls back to `PATH`
/// elsewhere -- and, if that file is not there, still falls back to `PATH`
/// rather than deciding the host cannot run the suite.
fn bash() -> PathBuf {
    if cfg!(windows) {
        let git_bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        if git_bash.is_file() {
            return git_bash;
        }
    }
    PathBuf::from("bash")
}

/// How a tier is spelled: label, log name, line budget, byte budget, then
/// the command.
struct Tier<'a> {
    label: &'a str,
    log: &'a str,
    max_lines: &'a str,
    max_bytes: &'a str,
    command: &'a [&'a str],
}

/// Run `scripts/tier.sh` from the repository root.
///
/// FROM THE ROOT, WITH A RELATIVE SCRIPT PATH, because this suite runs on
/// windows-latest, where `bash` is Git Bash and an absolute Windows path
/// handed to it as an argument is a path with backslashes in it. A relative
/// path plus a working directory is the one spelling that means the same
/// thing on all three runners.
///
/// `core_root` is what `FS_CORE_ROOT` should say for this run: `None`
/// inherits the environment, which is how the positive cases reach whatever
/// rust-fs-core this checkout is arranged against, and `Some(path)` is how
/// the refusals point the resolver at a scratch directory.
fn run_tier(tier: &Tier, core_root: Option<&Path>, verbose: bool) -> Output {
    let mut command = Command::new(bash());
    command
        .current_dir(repo())
        .arg("scripts/tier.sh")
        .args([tier.label, tier.log, tier.max_lines, tier.max_bytes, "--"])
        .args(tier.command);
    if let Some(root) = core_root {
        command.env("FS_CORE_ROOT", root);
    }
    if verbose {
        // Through CLI_ARGS, the way `chore test -- --verbose` arrives, so
        // this exercises tier.sh's own mapping rather than setting the
        // variable the wrapper reads and proving nothing.
        command.env("CLI_ARGS", " --verbose ");
    }
    let _serialised = ONE_TIER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    command.output().unwrap_or_else(|e| {
        panic!(
            "could not run `{} scripts/tier.sh`: {e}. Every test tier in this \
             repository runs through that script, so a host without `bash` \
             cannot run this suite -- which is why this is a failure and not \
             a skip. On Windows, Git Bash provides it.",
            bash().display()
        )
    })
}

/// Everything the run printed, on either stream.
fn printed(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string() + &String::from_utf8_lossy(&output.stderr)
}

/// A tier log path, relative to the repository root. The names are prefixed
/// so a test run cannot overwrite the log of a real tier.
fn log_name(name: &str) -> String {
    format!("budget-test-{name}")
}

fn log_contents(name: &str) -> String {
    let relative = format!("tmp/logs/{}.log", log_name(name));
    std::fs::read_to_string(repo().join(&relative))
        .unwrap_or_else(|e| panic!("the tier left no log at {relative}: {e}"))
}

/// A scratch directory under `tmp/`, which is gitignored.
fn scratch(name: &str) -> PathBuf {
    let directory = repo().join("tmp").join("resolver-test").join(name);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(directory.join("scripts"))
        .unwrap_or_else(|e| panic!("could not create {}: {e}", directory.display()));
    directory
}

/// A command that prints 40 lines and succeeds.
const LOUD: &[&str] = &[
    "sh",
    "-c",
    "i=0; while [ $i -lt 40 ]; do echo line $i; i=$((i+1)); done",
];

#[test]
fn a_passing_tier_is_quiet_names_its_log_and_keeps_everything() {
    let output = run_tier(
        &Tier {
            label: "quiet",
            log: &log_name("quiet"),
            max_lines: "5",
            max_bytes: "0",
            command: &["echo", "hello"],
        },
        None,
        false,
    );

    let printed = printed(&output);
    assert!(
        output.status.success(),
        "a tier inside its budget failed ({:?}). The wrapper is resolved from \
         rust-fs-core at run time: if this says the resolver found nothing, \
         check out rust-fs-core v0.2.13 or later beside this repository, or \
         set FS_CORE_ROOT to one.\n{printed}",
        output.status.code()
    );
    assert!(
        !printed.contains("hello"),
        "a passing tier put the command's output on the terminal, which is \
         the whole thing the budget exists to stop:\n{printed}"
    );
    assert!(
        printed.contains("quiet: ok"),
        "a passing tier printed no verdict line, so the reader is told \
         nothing at all -- quiet is not the same as silent:\n{printed}"
    );
    assert!(
        printed.contains(&log_name("quiet")),
        "the verdict line does not name the log, so the output that was \
         withheld cannot be found:\n{printed}"
    );
    assert_eq!(
        log_contents("quiet").trim(),
        "hello",
        "the tier log did not keep the command's output. The budget is not a \
         gag: everything goes to the log whatever happens to the terminal."
    );
}

/// The wrapper copies currently sitting in `tmp/`.
///
/// There is usually one that is none of our business: the whole suite runs
/// INSIDE a tier, so the outer `tier.sh` is holding its own copy for as long
/// as `cargo test` runs. The leak test therefore compares before with after
/// rather than asserting the directory is empty.
fn wrapper_copies() -> Vec<String> {
    let tmp = repo().join("tmp");
    let mut names: Vec<String> = std::fs::read_dir(&tmp)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.starts_with("output-budget.") && name.ends_with(".sh"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn the_copy_of_the_wrapper_does_not_outlive_the_run() {
    // tier.sh copies core's script into tmp/ for the run and removes it on
    // exit. If the copy were left behind, this repository would grow a
    // vendored wrapper again by accident -- an untracked one, which is worse
    // than the committed copy that was deleted, because nothing would show
    // it drifting.
    let before = wrapper_copies();
    let output = run_tier(
        &Tier {
            label: "copy",
            log: &log_name("copy"),
            max_lines: "5",
            max_bytes: "0",
            command: &["echo", "hello"],
        },
        None,
        false,
    );
    assert!(
        output.status.success(),
        "the tier failed: {}",
        printed(&output)
    );

    let after = wrapper_copies();
    let left_behind: Vec<_> = after.iter().filter(|name| !before.contains(name)).collect();
    assert!(
        left_behind.is_empty(),
        "the wrapper copy outlived the run: tmp/{left_behind:?}. It is \
         removed by a trap precisely so this repository never accumulates a \
         copy of a file it deliberately does not vendor."
    );
}

#[test]
fn a_failing_tier_hands_on_the_commands_own_status() {
    let output = run_tier(
        &Tier {
            label: "bad",
            log: &log_name("bad"),
            max_lines: "5",
            max_bytes: "0",
            command: &["sh", "-c", "echo the reason; exit 7"],
        },
        None,
        false,
    );

    assert_eq!(
        output.status.code(),
        Some(7),
        "a failing tier must exit with the COMMAND's status, not the \
         wrapper's. This is the `cargo test | tee log` defect that \
         tests/ci_profile.rs refuses in the workflow -- a red suite reading \
         green because something else in the pipeline succeeded.\n{}",
        printed(&output)
    );
    let printed = printed(&output);
    assert!(
        printed.contains(&log_name("bad")),
        "a failing tier did not name the log holding the reason. The tail is \
         opt-in (OUTPUT_BUDGET_FAIL_TAIL), so the one line naming the log is \
         all a reader gets and it has to be there:\n{printed}"
    );
    assert_eq!(
        log_contents("bad").trim(),
        "the reason",
        "a failing tier lost the output it was supposed to keep"
    );
}

#[test]
fn a_tier_over_its_line_budget_exits_65() {
    let output = run_tier(
        &Tier {
            label: "loud",
            log: &log_name("loud"),
            max_lines: "5",
            max_bytes: "0",
            command: LOUD,
        },
        None,
        false,
    );

    assert_eq!(
        output.status.code(),
        Some(65),
        "a tier over its line budget must exit 65 -- a status of its own, so \
         a suite that printed too much is not mistaken for a suite that \
         failed.\n{}",
        printed(&output)
    );
    assert_eq!(
        log_contents("loud").lines().count(),
        40,
        "a breached budget lost the output"
    );
}

#[test]
fn a_tier_over_its_byte_budget_exits_65_too() {
    let output = run_tier(
        &Tier {
            label: "fat",
            log: &log_name("fat"),
            max_lines: "0",
            max_bytes: "10",
            command: &["echo", "rather more than ten bytes"],
        },
        None,
        false,
    );

    assert_eq!(
        output.status.code(),
        Some(65),
        "a tier over its BYTE budget passed. Lines and bytes are two budgets \
         because one long line is not one line's worth of reading.\n{}",
        printed(&output)
    );
}

#[test]
fn cli_args_verbose_streams_the_run_and_does_not_lift_the_budget() {
    // THE ONE MECHANICAL CHECK ON THE RENAME. `tier.sh` maps `--verbose` in
    // CLI_ARGS onto an environment variable the wrapper reads, and that
    // variable is `OUTPUT_BUDGET_VERBOSE` since the wrapper became
    // rust-fs-core's. Exporting the old `FLTH_VERBOSE` would not error: the
    // canonical script does not read it, and the run would simply stay
    // quiet. Nothing but this test would notice.
    let output = run_tier(
        &Tier {
            label: "verbose",
            log: &log_name("verbose"),
            max_lines: "5",
            max_bytes: "0",
            command: &["echo", "hello"],
        },
        None,
        true,
    );
    let streamed = printed(&output);
    assert!(
        streamed.contains("hello"),
        "`chore test -- --verbose` did not stream the run. The quiet default \
         is only acceptable because asking for everything is one flag away -- \
         and the likeliest cause is tier.sh exporting the old FLTH_VERBOSE, \
         which the canonical wrapper does not read:\n{streamed}"
    );

    let output = run_tier(
        &Tier {
            label: "verbose-loud",
            log: &log_name("verbose-loud"),
            max_lines: "5",
            max_bytes: "0",
            command: LOUD,
        },
        None,
        true,
    );
    assert_eq!(
        output.status.code(),
        Some(65),
        "--verbose lifted the budget. It must not: the log is the same size \
         whether or not anybody was watching, and a tier that has outgrown \
         its budget should say so either way."
    );
}

#[test]
fn a_budget_of_zero_is_no_budget_which_is_why_the_workflow_may_not_use_one() {
    // Documented here because it is the reason
    // `every_output_budget_is_a_number_that_can_fail_a_run` in
    // tests/ci_profile.rs exists: a tier given `0` looks wrapped, logged and
    // compliant, and is unbounded.
    let output = run_tier(
        &Tier {
            label: "zero",
            log: &log_name("zero"),
            max_lines: "0",
            max_bytes: "0",
            command: LOUD,
        },
        None,
        false,
    );
    assert!(
        output.status.success(),
        "0 is meant to mean \"no budget\"; if that ever changes, the guard in \
         tests/ci_profile.rs that refuses a zero budget is measuring the \
         wrong thing and should change with it.\n{}",
        printed(&output)
    );
}

#[test]
fn the_resolver_refuses_a_core_that_does_not_hold_the_wrapper() {
    let empty = scratch("absent");
    let output = run_tier(
        &Tier {
            label: "absent",
            log: &log_name("absent"),
            max_lines: "5",
            max_bytes: "0",
            command: &["echo", "this must not run"],
        },
        Some(&empty),
        false,
    );

    let printed = printed(&output);
    assert!(
        !output.status.success(),
        "the resolver accepted a rust-fs-core with no scripts/output-budget.sh \
         and ran the tier anyway. A tier that cannot find the wrapper has no \
         budget, no log and no verdict, and must refuse rather than run \
         unmeasured:\n{printed}"
    );
    assert!(
        !printed.contains("this must not run"),
        "the tier's command ran despite the wrapper being unresolvable:\n{printed}"
    );
    assert!(
        printed.contains("FS_CORE_ROOT"),
        "the refusal did not name the setting that caused it, so nobody can \
         fix it:\n{printed}"
    );
    assert!(
        printed.contains("output-budget.sh"),
        "the refusal did not name the file it was looking for:\n{printed}"
    );
}

#[test]
fn the_resolver_refuses_a_wrapper_that_answers_the_wrong_api_version() {
    // A file in the right place, executable, that answers `--version` with
    // something else. This is the case a path check alone would wave
    // through: the resolver has to ASK, and a wrong answer has to be fatal
    // rather than a reason to go looking elsewhere -- falling through would
    // mean the tiers silently ran against a different wrapper than the one
    // the checkout supplies.
    let impostor = scratch("impostor");
    let script = impostor.join("scripts").join("output-budget.sh");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then
    echo 'some-other-output-budget 99'
    exit 0
fi
echo 'the impostor ran the command'
exit 0
"#,
    )
    .expect("could not write the impostor wrapper");

    let output = run_tier(
        &Tier {
            label: "impostor",
            log: &log_name("impostor"),
            max_lines: "5",
            max_bytes: "0",
            command: &["echo", "this must not run"],
        },
        Some(&impostor),
        false,
    );

    let printed = printed(&output);
    assert!(
        !output.status.success(),
        "the resolver accepted a wrapper that does not speak its API. The \
         version string is the whole contract -- this repository checks it \
         rather than a SHA-256 precisely so that a behaviour change is \
         caught while a comment change is not:\n{printed}"
    );
    assert!(
        !printed.contains("the impostor ran the command"),
        "the impostor was run rather than refused:\n{printed}"
    );
    assert!(
        printed.contains("rust-fs-core-output-budget 1"),
        "the refusal did not say which API string was expected:\n{printed}"
    );
    assert!(
        printed.contains("some-other-output-budget 99"),
        "the refusal did not say what the wrapper actually answered:\n{printed}"
    );
}
