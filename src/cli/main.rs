//! `rust-img-vmdk`: the command-line tool for VMDK images, one multi-call
//! binary.
//!
//! Installed as `rust-img-vmdk` and linked as `img.vmdk`; see `common` for
//! the dispatch and the output contract every tool shares, and `vmdk` for
//! the tool itself.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece this repository does not call yet is not dead,
// it is the part another format's tools will.
#[allow(dead_code)]
mod common;
mod vmdk;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
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
    common::main(&FAMILY)
}
