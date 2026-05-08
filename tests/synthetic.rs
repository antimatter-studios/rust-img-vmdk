//! End-to-end tests built around hand-crafted monolithic-sparse VMDK
//! fixtures.
//!
//! Each fixture is laid down sector-by-sector. Layout:
//!
//! ```text
//!   sector 0       sparse extent header (512 bytes)
//!   sector 1       embedded descriptor text (1 sector)
//!   sector 2       primary grain directory (1 entry → padded sector)
//!   sectors 3..6   grain table 0 (512 entries × 4 bytes = 4 sectors)
//!   sectors 7..134 grain 0 data (128 sectors = 64 KiB)
//! ```
//!
//! Virtual capacity = 1 MiB = 2048 sectors. `grain_size` = 128 sectors
//! (64 KiB). With `num_gtes_per_gt` = 512 we need a single grain table
//! and a one-entry grain directory — enough to exercise both the
//! "allocated grain" and "unallocated grain → zero" paths.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use vmdk::header::{HEADER_SIZE, MAGIC};
use vmdk::VmdkReader;

const SECTOR: u64 = 512;

fn tmp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("vmdk_synth_{}_{n}_{name}.vmdk", std::process::id()));
    p
}

trait WriteAt {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()>;
}
impl WriteAt for File {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(buf)
    }
}

/// Layout constants shared by every fixture. Values chosen to make the
/// math obvious — single GT, grain 0 lands at a clean sector.
const CAPACITY_SECTORS: u64 = 2048; // 1 MiB
const GRAIN_SIZE: u64 = 128;        // 64 KiB
const NUM_GTES_PER_GT: u32 = 512;
const DESC_OFF_SECTOR: u64 = 1;
const DESC_SIZE_SECTORS: u64 = 1;
const GD_OFF_SECTOR: u64 = 2;
const GT_OFF_SECTOR: u64 = 3; // sectors 3..6
const GRAIN0_OFF_SECTOR: u64 = 7; // first grain at sector 7

fn build_header() -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes()); // version
    h[8..12].copy_from_slice(&0u32.to_le_bytes()); // flags
    h[12..20].copy_from_slice(&CAPACITY_SECTORS.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&DESC_OFF_SECTOR.to_le_bytes());
    h[36..44].copy_from_slice(&DESC_SIZE_SECTORS.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[48..56].copy_from_slice(&0u64.to_le_bytes()); // rgd_offset (none)
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes()); // overhead
    // Standard VMDK end-of-line markers (cosmetic; reader doesn't care).
    h[73] = b'\n';
    h[74] = b' ';
    h[75] = b'\r';
    h[76] = b'\n';
    // compress_algorithm @ 77..79 = 0 (none).
    h
}

fn build_descriptor_sector() -> [u8; SECTOR as usize] {
    let text = "# Disk DescriptorFile\n\
                version=1\n\
                CID=fffffffe\n\
                parentCID=ffffffff\n\
                createType=\"monolithicSparse\"\n\
                \n\
                RW 2048 SPARSE \"synthetic.vmdk\"\n\
                \n\
                ddb.adapterType = \"ide\"\n\
                ddb.geometry.cylinders = \"2\"\n\
                ddb.geometry.heads = \"16\"\n\
                ddb.geometry.sectors = \"63\"\n";
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    s
}

/// Build a 1 MiB sparse VMDK. If `allocate_grain0` is true, grain 0 is
/// linked from the grain table and filled with `grain_pattern`. Otherwise
/// no grains are allocated (every read should return zero).
fn build_vmdk(path: &PathBuf, allocate_grain0: bool, grain_pattern: &[u8]) {
    let header = build_header();
    let descriptor = build_descriptor_sector();

    // Grain directory: 1 entry pointing at GT 0.
    let mut gd_sector = [0u8; SECTOR as usize];
    let gt_sector_le = (GT_OFF_SECTOR as u32).to_le_bytes();
    gd_sector[0..4].copy_from_slice(&gt_sector_le);

    // Grain table: 512 entries × 4 bytes = 2048 bytes (4 sectors).
    let mut gt_bytes = vec![0u8; (NUM_GTES_PER_GT as usize) * 4];
    if allocate_grain0 {
        let g0_le = (GRAIN0_OFF_SECTOR as u32).to_le_bytes();
        gt_bytes[0..4].copy_from_slice(&g0_le);
    }

    // Grain data — 128 sectors = 65536 bytes.
    let grain_bytes = (GRAIN_SIZE * SECTOR) as usize;
    assert_eq!(grain_pattern.len(), grain_bytes);

    // Total file length: at minimum needs to cover end of grain 0.
    let end_of_grain0 = (GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR;

    let mut f = File::create(path).unwrap();
    f.set_len(end_of_grain0).unwrap();
    f.write_all_at(&header, 0).unwrap();
    f.write_all_at(&descriptor, DESC_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&gd_sector, GD_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&gt_bytes, GT_OFF_SECTOR * SECTOR).unwrap();
    if allocate_grain0 {
        f.write_all_at(grain_pattern, GRAIN0_OFF_SECTOR * SECTOR)
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn opens_and_reports_virtual_size() {
    let path = tmp_path("size");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_vmdk(&path, true, &pattern);

    let r = VmdkReader::open(&path).unwrap();
    assert_eq!(r.virtual_size(), CAPACITY_SECTORS * SECTOR);
    assert_eq!(r.grain_size_bytes(), GRAIN_SIZE * SECTOR);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn allocated_grain_round_trip() {
    let path = tmp_path("alloc");
    let grain_bytes = (GRAIN_SIZE * SECTOR) as usize;
    let pattern: Vec<u8> = (0u8..=255u8).cycle().take(grain_bytes).collect();
    build_vmdk(&path, true, &pattern);

    let r = VmdkReader::open(&path).unwrap();

    // Read the entire grain back.
    let mut buf = vec![0u8; grain_bytes];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, pattern);

    // A short read crossing two sectors inside the grain.
    let mut buf2 = vec![0u8; 200];
    r.read_at(500, &mut buf2).unwrap();
    assert_eq!(&buf2[..], &pattern[500..700]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn unallocated_grain_reads_zero() {
    let path = tmp_path("zero");
    let pattern = vec![0xFFu8; (GRAIN_SIZE * SECTOR) as usize];
    // Allocate grain 0 only — every other grain in the GT is null.
    build_vmdk(&path, true, &pattern);

    let r = VmdkReader::open(&path).unwrap();

    // Grain 1 starts at virtual byte 64 KiB and is unallocated.
    let mut buf = vec![0xAAu8; 4096];
    r.read_at(GRAIN_SIZE * SECTOR, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "unallocated grain must read as zero");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn whole_image_unallocated_reads_zero() {
    // No grains allocated at all — exercises the "gt_sector != 0 but
    // gt[gte] == 0" path AND happens to also exercise the per-grain
    // zero-fill loop.
    let path = tmp_path("all_zero");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_vmdk(&path, false, &pattern);

    let r = VmdkReader::open(&path).unwrap();
    let mut buf = vec![0xCDu8; 8192];
    r.read_at(0, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_past_end_errors() {
    let path = tmp_path("oob");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_vmdk(&path, true, &pattern);

    let r = VmdkReader::open(&path).unwrap();
    let mut buf = [0u8; 16];
    let err = r
        .read_at(CAPACITY_SECTORS * SECTOR - 8, &mut buf)
        .unwrap_err();
    assert!(matches!(err, vmdk::Error::OutOfBounds { .. }));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rejects_non_monolithic_sparse() {
    // Same layout but the descriptor says monolithicFlat. Should fail
    // open with Unsupported.
    let path = tmp_path("flat");
    let header = build_header();

    let mut desc = [0u8; SECTOR as usize];
    let text = "createType=\"monolithicFlat\"\nRW 2048 FLAT \"x.vmdk\" 0\n";
    desc[..text.len()].copy_from_slice(text.as_bytes());

    let mut f = File::create(&path).unwrap();
    f.set_len((GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR).unwrap();
    f.write_all_at(&header, 0).unwrap();
    f.write_all_at(&desc, DESC_OFF_SECTOR * SECTOR).unwrap();
    drop(f);

    match VmdkReader::open(&path) {
        Err(vmdk::Error::Unsupported(_)) => {}
        Err(other) => panic!("expected Unsupported, got {other}"),
        Ok(_) => panic!("expected Unsupported, got Ok"),
    }

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// fs_core::BlockRead bridge sanity
// ---------------------------------------------------------------------------

#[test]
fn fs_core_blockread_size_matches_virtual() {
    let path = tmp_path("fs_core");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_vmdk(&path, true, &pattern);

    let r = VmdkReader::open(&path).unwrap();
    assert_eq!(
        <VmdkReader as fs_core::BlockRead>::size_bytes(&r),
        CAPACITY_SECTORS * SECTOR
    );

    let _ = std::fs::remove_file(&path);
}
