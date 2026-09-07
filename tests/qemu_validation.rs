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

/// A temp *directory* that removes itself on drop.
///
/// The single-file `TempPath` is not enough for the subformats that are
/// more than one file: `monolithicFlat` writes a descriptor plus a
/// `-flat.vmdk`, and the `twoGbMaxExtent*` pair write a descriptor plus
/// numbered extents. Cleaning those up by name means knowing qemu's
/// naming scheme; a directory does not.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("vmdk_qemu_{}_{n}_{name}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create fixture directory");
        TempDir(p)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Create an image in `subformat` and hand back the path qemu would
/// expect a caller to open — the descriptor file.
fn qemu_create_subformat(dir: &TempDir, subformat: &str, size: &str) -> PathBuf {
    let path = dir.join(&format!("{subformat}.vmdk"));
    assert_qemu(&[
        "create",
        "-f",
        "vmdk",
        "-o",
        &format!("subformat={subformat}"),
        path.to_str().unwrap(),
        size,
    ]);
    path
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

/// A flat or split VMDK is a descriptor *file*: a few hundred bytes of
/// plain text naming the extents that hold the data. There is no `KDMV`
/// magic anywhere in it, so the sparse-header parse fails first and the
/// image is reported as not a VMDK at all.
///
/// That is false, and false in the direction that hurts. A caller
/// probing a file against several container readers takes "not a VMDK"
/// as permission to move on, and ends up telling the user their VMDK is
/// unrecognised — for a file whose first line reads
/// `# Disk DescriptorFile`. `Unsupported` naming the create type tells
/// them which conversion to run instead.
///
/// None of these are exotic: `monolithicFlat` is what
/// `qemu-img create -f vmdk` produces with no `-o subformat=`, and the
/// `twoGbMaxExtent*` pair is what anything targeting older VMware or
/// FAT32 media produces.
#[test]
fn a_flat_or_split_descriptor_names_its_create_type_rather_than_denying_it_is_a_vmdk() {
    for subformat in [
        "monolithicFlat",
        "twoGbMaxExtentSparse",
        "twoGbMaxExtentFlat",
    ] {
        let dir = TempDir::new(subformat);
        let path = qemu_create_subformat(&dir, subformat, "8M");
        // qemu reads the set it just wrote, so the fixture is sound.
        assert_eq!(qemu_virtual_size(&path), 8 * 1024 * 1024);

        match VmdkReader::open(&path) {
            Err(vmdk::Error::Unsupported(msg)) => assert!(
                msg.contains(subformat),
                "the refusal must name the create type; for {subformat} it said {msg:?}"
            ),
            Err(other) => panic!("{subformat}: expected Unsupported, got {other}"),
            Ok(_) => panic!("{subformat}: opened a layout this crate cannot read"),
        }
    }
}

/// Which sparse-extent revisions this crate accepts is a question about
/// real files, not about the spec, so it is pinned against real files.
///
/// `qemu-img` writes version 1 for a plain monolithic sparse extent and
/// **2** for the same image when it carries the zeroed-grain marker —
/// which this crate reads, and has two tests above for. Accepting only
/// version 1, which is what the header field's own documentation would
/// suggest, would refuse images this very suite produces.
///
/// Version 3 is the stream-optimized revision and is refused. It was
/// already refused in practice, but by `compressAlgorithm` rather than
/// by the version — the one place the format states the same fact twice,
/// which leaves an image making only the first statement to walk
/// through.
#[test]
fn the_revisions_qemu_writes_are_the_revisions_we_accept() {
    let dir = TempDir::new("versions");
    let raw = dir.join("src.raw");
    std::fs::write(&raw, pattern(2 * 1024 * 1024)).unwrap();

    let plain = dir.join("plain.vmdk");
    qemu_convert_raw_to_vmdk(&raw, &plain);
    assert_eq!(VmdkReader::open(&plain).unwrap().header().version, 1);

    let zeroed = dir.join("zeroed.vmdk");
    assert_qemu(&[
        "convert",
        "-f",
        "raw",
        "-O",
        "vmdk",
        "-o",
        "subformat=monolithicSparse,zeroed_grain=on",
        raw.to_str().unwrap(),
        zeroed.to_str().unwrap(),
    ]);
    let r = VmdkReader::open(&zeroed).expect("a zeroed-grain image is version 2 and readable");
    assert_eq!(r.header().version, 2);
    let mut buf = vec![0u8; 4096];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, pattern(4096));

    let stream = dir.join("stream.vmdk");
    assert_qemu(&[
        "convert",
        "-f",
        "raw",
        "-O",
        "vmdk",
        "-o",
        "subformat=streamOptimized",
        raw.to_str().unwrap(),
        stream.to_str().unwrap(),
    ]);
    match VmdkReader::open(&stream) {
        Err(vmdk::Error::Unsupported(msg)) => assert!(
            msg.contains("version 3"),
            "a stream-optimized extent must be refused by its revision, got {msg:?}"
        ),
        Err(other) => panic!("expected Unsupported, got {other}"),
        Ok(_) => panic!("opened a stream-optimized extent"),
    }
}

/// A file that is neither a sparse extent nor a descriptor is still not
/// a VMDK. Widening the door for descriptor files must not turn the
/// probe into one that accepts anything.
#[test]
fn a_file_that_is_neither_a_sparse_extent_nor_a_descriptor_is_still_not_a_vmdk() {
    let dir = TempDir::new("not-a-vmdk");
    let path = dir.join("plain.bin");
    std::fs::write(&path, vec![0x5Au8; 4096]).unwrap();
    match VmdkReader::open(&path) {
        Err(vmdk::Error::NotVmdk) => {}
        Err(other) => panic!("expected NotVmdk, got {other}"),
        Ok(_) => panic!("expected NotVmdk, got Ok"),
    }
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

/// Both grain directories of an image, and the grain tables they name.
///
/// A sparse VMDK carries a second, redundant copy of the directory and
/// of every table, and announces it through bit 1 of the header's
/// `flags`. qemu and VMware both set that bit on every sparse extent
/// they produce, so this is the ordinary layout rather than an exotic
/// one.
struct Directories {
    /// Which directory slots hold a table, primary then redundant. The
    /// two directories name *different* tables by design, so their raw
    /// contents differ; what has to agree is which slots are populated.
    primary_allocated: Vec<bool>,
    redundant_allocated: Vec<bool>,
    /// The grain tables themselves, one entry list per directory slot.
    primary_gts: Vec<Vec<u32>>,
    redundant_gts: Vec<Vec<u32>>,
}

fn read_directories(path: &Path) -> Directories {
    let image = std::fs::read(path).unwrap();
    let u32_at = |off: usize| u32::from_le_bytes(image[off..off + 4].try_into().unwrap());
    let u64_at = |off: usize| u64::from_le_bytes(image[off..off + 8].try_into().unwrap());

    let capacity = u64_at(12);
    let grain_size = u64_at(20);
    let num_gtes_per_gt = u32_at(44) as u64;
    let rgd_offset = u64_at(48);
    let gd_offset = u64_at(56);
    assert!(
        u32_at(8) & 0x2 != 0 && rgd_offset != 0,
        "fixture precondition: the image must declare a redundant grain directory"
    );

    let gt_count = capacity.div_ceil(grain_size).div_ceil(num_gtes_per_gt) as usize;
    let sector = 512usize;

    let entries_at = |sector_no: usize, count: usize| -> Vec<u32> {
        let at = sector_no * sector;
        image[at..at + count * 4]
            .chunks_exact(4)
            .map(|e| u32::from_le_bytes(e.try_into().unwrap()))
            .collect()
    };
    let tables_of = |gd: &[u32]| -> Vec<Vec<u32>> {
        gd.iter()
            .map(|&s| {
                if s == 0 {
                    Vec::new()
                } else {
                    entries_at(s as usize, num_gtes_per_gt as usize)
                }
            })
            .collect()
    };

    let primary_gd = entries_at(gd_offset as usize, gt_count);
    let redundant_gd = entries_at(rgd_offset as usize, gt_count);
    Directories {
        primary_allocated: primary_gd.iter().map(|&s| s != 0).collect(),
        redundant_allocated: redundant_gd.iter().map(|&s| s != 0).collect(),
        primary_gts: tables_of(&primary_gd),
        redundant_gts: tables_of(&redundant_gd),
    }
}

/// Report the first grain-table entry on which the two copies disagree.
/// Comparing 512-entry tables with `assert_eq!` prints both in full,
/// which buries the one number that differs.
fn assert_tables_agree(d: &Directories, whose: &str) {
    assert_eq!(
        d.primary_allocated, d.redundant_allocated,
        "{whose}: one directory has a grain table where the other has none"
    );
    for (t, (p, r)) in d.primary_gts.iter().zip(&d.redundant_gts).enumerate() {
        if let Some(i) = p.iter().zip(r).position(|(a, b)| a != b) {
            panic!(
                "{whose}: grain table {t} entry {i} is {} in the primary copy \
                 and {} in the redundant one",
                p[i], r[i]
            );
        }
        assert_eq!(p.len(), r.len(), "{whose}: grain table {t} lengths differ");
    }
}

/// The redundant grain directory exists to be read when the primary is
/// damaged, so it has to say the same thing the primary does. A writer
/// that updates only the primary leaves the redundant copy claiming a
/// grain is absent when it is on disk — and a tool that falls back to it
/// then returns zeros for real data, from an image `qemu-img check`
/// calls clean, because `check` does not consult the redundant tables.
///
/// Doing the same write through qemu in the same test is what says the
/// shape we produce is the shape qemu produces, rather than merely
/// self-consistent.
#[test]
fn a_write_keeps_the_redundant_grain_directory_in_step() {
    let ours = vmdk_path("rgd-ours");
    let theirs = vmdk_path("rgd-theirs");
    qemu_create(&ours, "8M");
    std::fs::copy(&ours, &theirs).unwrap();

    const AT: u64 = 1024 * 1024;
    let payload = [0x33u8; 4096];

    let r = VmdkReader::open_rw(&ours).unwrap();
    r.write_at(AT, &payload).unwrap();
    r.flush().unwrap();
    drop(r);

    assert_qemu_io(&[
        "-f",
        "vmdk",
        "-c",
        &format!("write -P 0x33 {AT} {}", payload.len()),
        theirs.to_str().unwrap(),
    ]);

    assert_tables_agree(
        &read_directories(&theirs),
        "precondition (qemu's own write)",
    );
    assert_tables_agree(&read_directories(&ours), "after our write");

    qemu_check(&ours);
    let raw = raw_path("rgd-ours");
    qemu_convert_vmdk_to_raw(&ours, &raw);
    let out = std::fs::read(&raw).unwrap();
    assert_eq!(&out[AT as usize..AT as usize + payload.len()], &payload[..]);
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
