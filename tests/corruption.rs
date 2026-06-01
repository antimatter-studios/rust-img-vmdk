//! Reader-level corruption and write-persistence tests.
//!
//! Builds a valid 1 MiB monolithicSparse VMDK sector-by-sector, then
//! surgically corrupts a single header field (or the descriptor bytes)
//! to confirm the `open()` path rejects it cleanly. Also covers a
//! write→drop→reopen→read round-trip that the synthetic suite doesn't.
//!
//! Layout (identical to tests/synthetic.rs):
//!
//! ```text
//!   sector 0       sparse extent header
//!   sector 1       embedded descriptor
//!   sector 2       grain directory (1 entry)
//!   sectors 3..6   grain table 0 (512 entries)
//!   sectors 7..134 grain 0 data (64 KiB)
//! ```

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use vmdk::header::{HEADER_SIZE, MAGIC};
use vmdk::{Error, VmdkReader};

const SECTOR: u64 = 512;
const CAPACITY_SECTORS: u64 = 2048; // 1 MiB
const GRAIN_SIZE: u64 = 128; // 64 KiB
const NUM_GTES_PER_GT: u32 = 512;
const DESC_OFF_SECTOR: u64 = 1;
const DESC_SIZE_SECTORS: u64 = 1;
const GD_OFF_SECTOR: u64 = 2;
const GT_OFF_SECTOR: u64 = 3;
const GRAIN0_OFF_SECTOR: u64 = 7;
const FILE_LEN: u64 = (GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR;

// Header field offsets corrupted by individual tests.
const OFF_DESC_OFFSET: u64 = 28;
const OFF_DESC_SIZE: u64 = 36;
const OFF_GD_OFFSET: u64 = 56;

fn tmp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vmdk_corrupt_{}_{n}_{name}.vmdk",
        std::process::id()
    ));
    p
}

fn build_header() -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[12..20].copy_from_slice(&CAPACITY_SECTORS.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&DESC_OFF_SECTOR.to_le_bytes());
    h[36..44].copy_from_slice(&DESC_SIZE_SECTORS.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes());
    h
}

fn descriptor_sector() -> [u8; SECTOR as usize] {
    let text = "createType=\"monolithicSparse\"\nRW 2048 SPARSE \"corrupt.vmdk\"\n";
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    s
}

/// Write a valid monolithicSparse VMDK with grain 0 allocated.
fn build_valid(path: &PathBuf) {
    let mut f = File::create(path).unwrap();
    f.set_len(FILE_LEN).unwrap();
    write_at(&mut f, 0, &build_header());
    write_at(&mut f, DESC_OFF_SECTOR * SECTOR, &descriptor_sector());

    let mut gd = [0u8; SECTOR as usize];
    gd[0..4].copy_from_slice(&(GT_OFF_SECTOR as u32).to_le_bytes());
    write_at(&mut f, GD_OFF_SECTOR * SECTOR, &gd);

    let mut gt = vec![0u8; NUM_GTES_PER_GT as usize * 4];
    gt[0..4].copy_from_slice(&(GRAIN0_OFF_SECTOR as u32).to_le_bytes());
    write_at(&mut f, GT_OFF_SECTOR * SECTOR, &gt);

    let grain: Vec<u8> = (0..(GRAIN_SIZE * SECTOR))
        .map(|i| (i & 0xFF) as u8)
        .collect();
    write_at(&mut f, GRAIN0_OFF_SECTOR * SECTOR, &grain);
}

fn write_at(f: &mut File, off: u64, buf: &[u8]) {
    f.seek(SeekFrom::Start(off)).unwrap();
    f.write_all(buf).unwrap();
}

fn patch(path: &PathBuf, off: u64, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    write_at(&mut f, off, bytes);
    f.flush().unwrap();
}

#[test]
fn valid_image_opens_and_reads_grain() {
    let path = tmp_path("baseline");
    build_valid(&path);
    let r = VmdkReader::open(&path).unwrap();
    assert_eq!(r.virtual_size(), CAPACITY_SECTORS * SECTOR);
    let mut buf = [0u8; 8];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn missing_descriptor_offset_is_unsupported() {
    let path = tmp_path("no_desc");
    build_valid(&path);
    patch(&path, OFF_DESC_OFFSET, &0u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Unsupported(_)) => {}
        other => panic!("expected Unsupported, got {:?}", other.err()),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn descriptor_extending_past_eof_is_corrupt() {
    let path = tmp_path("desc_oob");
    build_valid(&path);
    patch(&path, OFF_DESC_SIZE, &100_000u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn zero_grain_directory_offset_is_corrupt() {
    let path = tmp_path("gd_zero");
    build_valid(&path);
    patch(&path, OFF_GD_OFFSET, &0u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn grain_directory_past_eof_is_corrupt() {
    let path = tmp_path("gd_oob");
    build_valid(&path);
    patch(&path, OFF_GD_OFFSET, &100_000u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn non_utf8_descriptor_is_corrupt() {
    let path = tmp_path("desc_binary");
    build_valid(&path);
    // Overwrite the descriptor sector with invalid UTF-8 bytes.
    patch(&path, DESC_OFF_SECTOR * SECTOR, &[0xFFu8; 64]);
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_into_sparse_grain_persists_across_reopen() {
    let path = tmp_path("persist");
    build_valid(&path);

    // Grain 0 is allocated; grain 1 (virtual offset 64 KiB) is sparse.
    // Writing into it must allocate a grain and survive a reopen.
    let virt = GRAIN_SIZE * SECTOR; // 64 KiB — start of grain 1
    let payload = [0x5Au8; 4096];
    {
        let r = VmdkReader::open_rw(&path).unwrap();
        r.write_at(virt + 1024, &payload).unwrap();
        r.flush().unwrap();
    }

    let r2 = VmdkReader::open(&path).unwrap();
    let mut got = [0u8; 4096];
    r2.read_at(virt + 1024, &mut got).unwrap();
    assert_eq!(got, payload);
    // Rest of the freshly-allocated grain reads as zero.
    let mut head = [0u8; 1024];
    r2.read_at(virt, &mut head).unwrap();
    assert!(head.iter().all(|&b| b == 0));
    let _ = std::fs::remove_file(&path);
}
