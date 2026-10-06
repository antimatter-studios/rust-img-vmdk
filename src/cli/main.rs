//! `rust-img-vmdk`: the command-line tool for VMDK images, one multi-call
//! binary.
//!
//! Installed as `rust-img-vmdk` and linked as `img.vmdk`. The dispatch and the
//! output contract every tool shares are `fs_core::cli` (rust-fs-core's `cli`
//! feature); `vmdk` is the tool itself.

mod vmdk;

use fs_core::cli;
use std::process::ExitCode;

static FAMILY: cli::Family = cli::Family {
    repo: "rust-img-vmdk",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "VMDK tools: report, read and write a VMDK disk image without a hypervisor",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-img-vmdk`",
    ],
    tools: &[vmdk::img::TOOL],
};

fn main() -> ExitCode {
    cli::main(&FAMILY)
}
