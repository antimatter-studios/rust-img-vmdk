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
const QEMU_IO: &str = "qemu-io";

fn run_qemu(args: &[&str]) -> std::process::Output {
    Command::new(QEMU_IMG)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke `{QEMU_IMG}` ({e}); install qemu-utils?"))
}

/// `qemu-io` ships in the same package as `qemu-img`. It is the only way
/// to get a *zeroed grain* into a fixture: `qemu-img convert` writes an
/// unallocated grain for a zero region, whereas an explicit zero write
/// into an image created with `zeroed_grain=on` writes the marker.
fn run_qemu_io(args: &[&str]) -> std::process::Output {
    Command::new(QEMU_IO)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke `{QEMU_IO}` ({e}); install qemu-utils?"))
}

fn assert_qemu_io(args: &[&str]) {
    let out = run_qemu_io(args);
    assert!(
        out.status.success(),
        "`qemu-io {}` failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
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

/// Build a VMDK whose grain tables can carry the zeroed-grain marker,
/// populate every grain from `raw`, then punch a zero region so one of
/// them becomes a marker rather than a pointer.
///
/// `zeroed_grain=on` sets bit 2 of the header's `flags` word; the
/// explicit `write -z` is what turns the grain's table entry into `1`.
fn qemu_image_with_a_zeroed_grain(vmdk: &Path, raw: &Path, hole_at: u64, hole_len: u64) {
    assert_qemu(&[
        "convert",
        "-f",
        "raw",
        "-O",
        "vmdk",
        "-o",
        "subformat=monolithicSparse,zeroed_grain=on",
        raw.to_str().unwrap(),
        vmdk.to_str().unwrap(),
    ]);
    assert_qemu_io(&[
        "-f",
        "vmdk",
        "-c",
        &format!("write -z {hole_at} {hole_len}"),
        vmdk.to_str().unwrap(),
    ]);
}

fn qemu_virtual_size(path: &Path) -> u64 {
    let out = run_qemu(&["info", "--output=json", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "qemu-img info failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr).trim(),
    );
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

/// Cross-read (zeroed grain): qemu punches a hole into an image created
/// with `zeroed_grain=on`, which puts the sentinel `1` in the grain
/// table. qemu reads that region as zeros; so must we.
///
/// Read as a host sector number instead, `1` addresses the embedded
/// descriptor — so the wrong answer here is not noise but the image's own
/// ASCII configuration text, returned with no error.
#[test]
fn our_reader_returns_zeros_for_a_grain_qemu_marked_zeroed() {
    let raw = raw_path("zg-src");
    let vmdk = vmdk_path("zg");
    const HOLE_AT: u64 = 1024 * 1024;
    const HOLE_LEN: u64 = 64 * 1024;

    let data = pattern(8 * 1024 * 1024);
    std::fs::write(&raw, &data).unwrap();
    qemu_image_with_a_zeroed_grain(&vmdk, &raw, HOLE_AT, HOLE_LEN);

    let r = VmdkReader::open(&vmdk).unwrap();
    let mut buf = vec![0xAAu8; HOLE_LEN as usize];
    r.read_at(HOLE_AT, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "a zeroed grain must read as zeros; first 64 bytes were {:?}",
        String::from_utf8_lossy(&buf[..64])
    );

    // Whole-image agreement, which is what a stacked consumer sees.
    let converted = raw_path("zg-out");
    qemu_convert_vmdk_to_raw(&vmdk, &converted);
    let theirs = std::fs::read(&converted).unwrap();
    let mut ours = vec![0u8; theirs.len()];
    r.read_at(0, &mut ours).unwrap();
    let first_diff = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    assert_eq!(
        first_diff, None,
        "our read disagrees with qemu-img convert at byte {first_diff:?}"
    );
}

/// Cross-write (zeroed grain): writing into a grain the table marks as
/// zeroed must allocate a grain, not write through to host sector 1.
///
/// The assertion that matters is the last one: writing over sector 1
/// destroys the embedded descriptor, after which `qemu-img` cannot open
/// the file at all.
#[test]
fn qemu_still_opens_an_image_after_we_write_into_a_zeroed_grain() {
    let raw = raw_path("zgw-src");
    let vmdk = vmdk_path("zgw");
    const HOLE_AT: u64 = 1024 * 1024;
    const HOLE_LEN: u64 = 64 * 1024;

    let data = pattern(8 * 1024 * 1024);
    std::fs::write(&raw, &data).unwrap();
    qemu_image_with_a_zeroed_grain(&vmdk, &raw, HOLE_AT, HOLE_LEN);

    let payload = vec![0x5Au8; 4096];
    let r = VmdkReader::open_rw(&vmdk).unwrap();
    r.write_at(HOLE_AT, &payload).unwrap();
    r.flush().unwrap();
    drop(r);

    // The image is still a VMDK.
    assert_eq!(qemu_virtual_size(&vmdk), data.len() as u64);
    qemu_check(&vmdk);

    // And qemu sees the bytes we wrote, with the rest of the punched
    // region still zero.
    let out_raw = raw_path("zgw-out");
    qemu_convert_vmdk_to_raw(&vmdk, &out_raw);
    let out = std::fs::read(&out_raw).unwrap();
    let at = HOLE_AT as usize;
    assert_eq!(&out[at..at + payload.len()], &payload[..]);
    assert!(
        out[at + payload.len()..at + HOLE_LEN as usize]
            .iter()
            .all(|&b| b == 0),
        "the untouched remainder of a zeroed grain must stay zero"
    );
    // The data outside the hole is untouched.
    assert_eq!(&out[..at], &data[..at]);
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
