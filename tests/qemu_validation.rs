//! Cross-validation against `qemu-img` (VMDK monolithicSparse).
//!
//! Gated behind the `qemu-validation` feature so regular `cargo test`
//! does not require qemu-img on PATH. Run with:
//!
//!     cargo test --features qemu-validation --test qemu_validation
//!
//! Licensing posture: `qemu-img` is invoked as a separate OS process.
//! No QEMU source or binary is linked into this crate, and `qemu-img`
//! is never bundled into a release artifact.
//!
//! The reader/writer only handles `monolithicSparse`, so every qemu
//! fixture is created with `-o subformat=monolithicSparse`. Three
//! directions are cross-checked:
//!
//!   1. cross-read   — qemu produces a VMDK, our reader consumes it
//!      (empty + populated).
//!   2. cross-write  — our writer mutates a qemu-created VMDK; qemu-img
//!      check then validates structure and convert extracts the bytes.
//!   3. metadata     — qemu-img info reports the virtual size we read.

#![cfg(feature = "qemu-validation")]

use std::path::{Path, PathBuf};
use std::process::Command;

use vmdk::VmdkReader;

const QEMU_IMG: &str = "qemu-img";

fn run_qemu(args: &[&str]) -> std::process::Output {
    Command::new(QEMU_IMG)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke `{QEMU_IMG}` ({e}); install qemu-utils?"))
}

fn assert_qemu(args: &[&str]) {
    let out = run_qemu(args);
    assert!(
        out.status.success(),
        "`qemu-img {}` failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn tmp(ext: &str, name: &str) -> TempPath {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("vmdk_qemu_{}_{n}_{name}.{ext}", std::process::id()));
    TempPath(p)
}

/// RAII temp-file path: removes the backing file on drop so a panicking
/// assertion can't leak fixtures into the temp dir across CI runs.
struct TempPath(PathBuf);
impl std::ops::Deref for TempPath {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}
impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn vmdk_path(name: &str) -> TempPath {
    tmp("vmdk", name)
}
fn raw_path(name: &str) -> TempPath {
    tmp("raw", name)
}

fn qemu_create(path: &Path, size: &str) {
    assert_qemu(&[
        "create",
        "-f",
        "vmdk",
        "-o",
        "subformat=monolithicSparse",
        path.to_str().unwrap(),
        size,
    ]);
}

fn qemu_check(path: &Path) {
    assert_qemu(&["check", path.to_str().unwrap()]);
}

fn qemu_convert_raw_to_vmdk(raw: &Path, vmdk: &Path) {
    assert_qemu(&[
        "convert",
        "-f",
        "raw",
        "-O",
        "vmdk",
        "-o",
        "subformat=monolithicSparse",
        raw.to_str().unwrap(),
        vmdk.to_str().unwrap(),
    ]);
}

fn qemu_convert_vmdk_to_raw(vmdk: &Path, raw: &Path) {
    assert_qemu(&[
        "convert",
        "-f",
        "vmdk",
        "-O",
        "raw",
        vmdk.to_str().unwrap(),
        raw.to_str().unwrap(),
    ]);
}

fn qemu_virtual_size(path: &Path) -> u64 {
    let out = run_qemu(&["info", "--output=json", path.to_str().unwrap()]);
    assert!(out.status.success(), "qemu-img info failed");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("info JSON parses");
    assert_eq!(v["format"], "vmdk");
    v["virtual-size"].as_u64().expect("virtual-size is u64")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn qemu_img_is_callable() {
    let out = run_qemu(&["--version"]);
    assert!(out.status.success(), "qemu-img --version exited non-zero");
}

/// Direction 1 (structural): qemu's own monolithicSparse passes its own
/// check on this host.
#[test]
fn qemu_check_passes_on_empty_qemu_image() {
    let p = vmdk_path("empty");
    qemu_create(&p, "4M");
    qemu_check(&p);
}

/// Cross-read (trivial): a blank qemu VMDK reads as zeros, and our
/// reader's virtual size and grain size match qemu's layout.
#[test]
fn our_reader_returns_zeros_and_geometry_for_empty_qemu_image() {
    let p = vmdk_path("zeros");
    qemu_create(&p, "4M");

    let r = VmdkReader::open(&p).unwrap();
    assert_eq!(r.virtual_size(), qemu_virtual_size(&p));
    assert_eq!(r.grain_size_bytes(), 64 * 1024); // qemu default grain

    let mut buf = vec![0u8; 65_536];
    r.read_at(0, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "empty VMDK must read zeros");
}

/// Cross-read (populated): convert a raw pattern into VMDK via qemu and
/// read it back with our reader. Exercises the GD/GT walk + grain read
/// against a real qemu sparse layout, including a multi-grain span.
#[test]
fn our_reader_matches_qemu_populated_pattern() {
    let raw = raw_path("pat-src");
    let vmdk = vmdk_path("pat-dst");

    // 300 KiB spans several 64 KiB grains.
    let data = pattern(300 * 1024);
    std::fs::write(&raw, &data).unwrap();
    qemu_convert_raw_to_vmdk(&raw, &vmdk);

    let r = VmdkReader::open(&vmdk).unwrap();
    let mut buf = vec![0u8; data.len()];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, data, "byte mismatch reading qemu-produced VMDK");
}

/// Cross-write (structural): our writer mutates a qemu-created VMDK,
/// then qemu-img check validates the grain directory / grain tables.
#[test]
fn qemu_check_passes_on_image_we_wrote_to() {
    let p = vmdk_path("we-wrote-check");
    qemu_create(&p, "4M");

    let r = VmdkReader::open_rw(&p).unwrap();
    // Write into a sparse grain so the writer must allocate a grain and
    // update the grain table.
    r.write_at(128 * 1024, b"vmdk written by our crate")
        .unwrap();
    r.flush().unwrap();
    drop(r);

    qemu_check(&p);
}

/// Cross-write (content): the strongest single check — write via our
/// crate, have qemu convert to raw, and verify the bytes survived.
#[test]
fn qemu_extracts_bytes_we_wrote() {
    let vmdk = vmdk_path("we-wrote-convert");
    let raw = raw_path("we-wrote-convert");
    qemu_create(&vmdk, "4M");

    let payload = b"bytes-qemu-must-see-back-0123456789";
    let off = 70_000; // mid-grain, unaligned
    let r = VmdkReader::open_rw(&vmdk).unwrap();
    r.write_at(off, payload).unwrap();
    r.flush().unwrap();
    drop(r);

    qemu_check(&vmdk);
    qemu_convert_vmdk_to_raw(&vmdk, &raw);
    let out = std::fs::read(&raw).unwrap();
    assert_eq!(&out[off as usize..off as usize + payload.len()], payload);
}
