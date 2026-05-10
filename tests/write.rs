//! Write-path tests for the monolithicSparse VMDK driver.
//!
//! Mirrors `tests/synthetic.rs` for fixture construction (same layout
//! constants), then exercises the four interesting write shapes:
//!
//! 1. On-device round-trip (open via `open_on_device`, read back via the
//!    BlockRead bridge).
//! 2. Write-through into an already-allocated grain.
//! 3. Write into a sparse grain (allocates + updates GT entry).
//! 4. Multi-grain write spanning allocated + sparse grains.
//! 5. Write into a grain whose grain-table cluster isn't allocated yet
//!    (allocates the GT, updates the GD, allocates the grain, updates
//!    the GT entry).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use fs_core::{BlockDevice, BlockRead, FileDevice};
use vmdk::header::{HEADER_SIZE, MAGIC};
use vmdk::VmdkReader;

const SECTOR: u64 = 512;

// Same layout constants the read-side fixture uses, so the two test
// files stay calibrated to each other.
const CAPACITY_SECTORS: u64 = 2048; // 1 MiB
const GRAIN_SIZE: u64 = 128; // 64 KiB
const NUM_GTES_PER_GT: u32 = 512;
const DESC_OFF_SECTOR: u64 = 1;
const DESC_SIZE_SECTORS: u64 = 1;
const GD_OFF_SECTOR: u64 = 2;
const GT_OFF_SECTOR: u64 = 3;
const GRAIN0_OFF_SECTOR: u64 = 7;

fn tmp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("vmdk_write_{}_{n}_{name}.vmdk", std::process::id()));
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

fn build_header() -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[12..20].copy_from_slice(&CAPACITY_SECTORS.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&DESC_OFF_SECTOR.to_le_bytes());
    h[36..44].copy_from_slice(&DESC_SIZE_SECTORS.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[48..56].copy_from_slice(&0u64.to_le_bytes());
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes());
    h[73] = b'\n';
    h[74] = b' ';
    h[75] = b'\r';
    h[76] = b'\n';
    h
}

fn build_descriptor_sector() -> [u8; SECTOR as usize] {
    let text = "# Disk DescriptorFile\n\
                version=1\n\
                CID=fffffffe\n\
                parentCID=ffffffff\n\
                createType=\"monolithicSparse\"\n\
                \n\
                RW 2048 SPARSE \"synthetic.vmdk\"\n";
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    s
}

/// Build a 1 MiB sparse VMDK whose **only** allocated cluster is grain 0
/// (linked from a populated grain table). Every other grain inside that
/// GT is sparse; the GD has just the one entry pointing at GT 0 — there
/// are no other GT slots to consider in this fixture (gt_count == 1).
fn build_grain0_only(path: &PathBuf, grain0_pattern: &[u8]) {
    let header = build_header();
    let descriptor = build_descriptor_sector();

    let mut gd_sector = [0u8; SECTOR as usize];
    gd_sector[0..4].copy_from_slice(&(GT_OFF_SECTOR as u32).to_le_bytes());

    let mut gt_bytes = vec![0u8; (NUM_GTES_PER_GT as usize) * 4];
    gt_bytes[0..4].copy_from_slice(&(GRAIN0_OFF_SECTOR as u32).to_le_bytes());

    let grain_bytes_n = (GRAIN_SIZE * SECTOR) as usize;
    assert_eq!(grain0_pattern.len(), grain_bytes_n);

    let end_of_grain0 = (GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR;

    let mut f = File::create(path).unwrap();
    f.set_len(end_of_grain0).unwrap();
    f.write_all_at(&header, 0).unwrap();
    f.write_all_at(&descriptor, DESC_OFF_SECTOR * SECTOR)
        .unwrap();
    f.write_all_at(&gd_sector, GD_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&gt_bytes, GT_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(grain0_pattern, GRAIN0_OFF_SECTOR * SECTOR)
        .unwrap();
}

/// Build a fully-sparse VMDK whose GD has TWO slots, both pointing at
/// nothing (entries == 0), and capacity wide enough that virtual offset
/// belonging to GT slot 1 is reachable. Used for the
/// "GT-itself-unallocated" test: a write into grain owned by GT[0]
/// must allocate GT[0] before allocating the grain.
///
/// To force the GD to be 2 entries we double the capacity so the grain
/// count straddles two GTs (entries_per_gt = 512, grain_size = 128
/// sectors → one GT covers 512*128 = 65_536 sectors = 32 MiB). Set
/// capacity to 64 MiB = 131_072 sectors so we get exactly 2 GTs.
fn build_fully_sparse_two_gts(path: &PathBuf) {
    const CAP: u64 = 131_072; // 64 MiB
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[12..20].copy_from_slice(&CAP.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&DESC_OFF_SECTOR.to_le_bytes());
    h[36..44].copy_from_slice(&DESC_SIZE_SECTORS.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes());
    h[73] = b'\n';
    h[74] = b' ';
    h[75] = b'\r';
    h[76] = b'\n';

    let descriptor = build_descriptor_sector();

    // GD with 2 entries, both zero. Pad to one sector.
    let gd_sector = [0u8; SECTOR as usize];

    // No GT, no grain data — file just needs to extend past the header
    // region so the alloc cursor sits somewhere sane.
    let initial_end = (GRAIN0_OFF_SECTOR) * SECTOR;
    let mut f = File::create(path).unwrap();
    f.set_len(initial_end).unwrap();
    f.write_all_at(&h, 0).unwrap();
    f.write_all_at(&descriptor, DESC_OFF_SECTOR * SECTOR)
        .unwrap();
    f.write_all_at(&gd_sector, GD_OFF_SECTOR * SECTOR).unwrap();
}

fn read_grain_sector_value(path: &PathBuf, gt_sector: u32, gte_idx: u64) -> u32 {
    let mut f = File::open(path).unwrap();
    let off = (gt_sector as u64) * SECTOR + gte_idx * 4;
    f.seek(SeekFrom::Start(off)).unwrap();
    let mut bytes = [0u8; 4];
    f.read_exact(&mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn read_gd_entry(path: &PathBuf, idx: u64) -> u32 {
    let mut f = File::open(path).unwrap();
    let off = GD_OFF_SECTOR * SECTOR + idx * 4;
    f.seek(SeekFrom::Start(off)).unwrap();
    let mut bytes = [0u8; 4];
    f.read_exact(&mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

// ---------------------------------------------------------------------------
// 1. on-device round-trip
// ---------------------------------------------------------------------------

#[test]
fn on_device_round_trip_reads_match_path_open() {
    let path = tmp_path("on_device");
    let pattern: Vec<u8> = (0u8..=255u8)
        .cycle()
        .take((GRAIN_SIZE * SECTOR) as usize)
        .collect();
    build_grain0_only(&path, &pattern);

    let dev = Arc::new(FileDevice::open(&path).unwrap()) as Arc<dyn BlockDevice>;
    let r = VmdkReader::open_on_device(dev).unwrap();
    assert!(!r.is_writable());
    assert_eq!(r.virtual_size(), CAPACITY_SECTORS * SECTOR);

    let mut buf = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, pattern);

    // BlockRead bridge sees the same bytes.
    let mut buf2 = vec![0u8; 32];
    <VmdkReader as BlockRead>::read_at(&r, 100, &mut buf2).unwrap();
    assert_eq!(buf2, pattern[100..132]);

    // Read-only on-device: writes must error.
    let err = r.write_at(0, &[1u8; 8]);
    assert!(matches!(err, Err(vmdk::Error::ReadOnly)));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 2. write-through into an allocated grain
// ---------------------------------------------------------------------------

#[test]
fn write_into_allocated_grain_is_writethrough() {
    let path = tmp_path("alloc_writethrough");
    let pattern: Vec<u8> = (0u8..=255u8)
        .cycle()
        .take((GRAIN_SIZE * SECTOR) as usize)
        .collect();
    build_grain0_only(&path, &pattern);

    let r = VmdkReader::open_rw(&path).unwrap();
    assert!(r.is_writable());

    let payload = [0xDEu8, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    r.write_at(2000, &payload).unwrap();
    r.flush().unwrap();

    // Read back through the same reader.
    let mut readback = [0u8; 8];
    r.read_at(2000, &mut readback).unwrap();
    assert_eq!(readback, payload);

    // And independently confirm the on-disk grain bytes mutated where
    // expected — grain 0 sits at sector 7, so virtual byte 2000 lands
    // at host byte 7*512 + 2000 = 5584.
    drop(r);
    let mut f = File::open(&path).unwrap();
    f.seek(SeekFrom::Start(7 * 512 + 2000)).unwrap();
    let mut on_disk = [0u8; 8];
    f.read_exact(&mut on_disk).unwrap();
    assert_eq!(on_disk, payload);

    // The GT entry for grain 0 must NOT have moved.
    assert_eq!(
        read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 0),
        GRAIN0_OFF_SECTOR as u32,
        "write-through must not relocate an allocated grain"
    );

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 3. write into a sparse grain (allocate + GT update)
// ---------------------------------------------------------------------------

#[test]
fn write_into_sparse_grain_allocates_and_updates_gt() {
    let path = tmp_path("sparse_alloc");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_grain0_only(&path, &pattern);

    // Grain 1 starts at virtual byte 64 KiB and is sparse in this fixture.
    let virt = GRAIN_SIZE * SECTOR; // 65_536
    let payload: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();

    // Pre-write: GT entry for grain 1 must be 0.
    assert_eq!(read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 1), 0);

    let r = VmdkReader::open_rw(&path).unwrap();
    r.write_at(virt + 100, &payload).unwrap();
    r.flush().unwrap();

    // Read back the payload.
    let mut readback = vec![0u8; payload.len()];
    r.read_at(virt + 100, &mut readback).unwrap();
    assert_eq!(readback, payload);

    // Bytes inside the same grain that we didn't write must read as zero
    // (the spec's "absent grain reads zero" carries over to a freshly
    // allocated grain — we zero-init on partial writes).
    let mut zeros = vec![0xFFu8; 100];
    r.read_at(virt, &mut zeros).unwrap();
    assert!(zeros.iter().all(|&b| b == 0), "head of grain must be zero");

    drop(r);

    // GT entry for grain 1 must now point at a newly allocated grain
    // past the end of grain 0.
    let new_grain_sector = read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 1);
    let end_of_grain0_sector = GRAIN0_OFF_SECTOR + GRAIN_SIZE;
    assert!(
        new_grain_sector as u64 >= end_of_grain0_sector,
        "new grain must land past existing data; got sector {new_grain_sector}"
    );

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 4. multi-grain write spanning allocated + sparse grains
// ---------------------------------------------------------------------------

#[test]
fn multi_grain_write_spans_allocated_and_sparse() {
    let path = tmp_path("multi_grain");
    let pattern: Vec<u8> = (0u8..=255u8)
        .cycle()
        .take((GRAIN_SIZE * SECTOR) as usize)
        .collect();
    build_grain0_only(&path, &pattern);

    let r = VmdkReader::open_rw(&path).unwrap();

    // Span the last 1024 bytes of grain 0 (allocated) and the first 1024
    // bytes of grain 1 (sparse → allocate). Total 2 KiB.
    let grain_bytes = (GRAIN_SIZE * SECTOR) as usize;
    let start = grain_bytes as u64 - 1024;
    let payload: Vec<u8> = (0u32..2048).map(|i| ((i ^ 0x5A) & 0xff) as u8).collect();
    r.write_at(start, &payload).unwrap();
    r.flush().unwrap();

    // Read back via the reader.
    let mut readback = vec![0u8; payload.len()];
    r.read_at(start, &mut readback).unwrap();
    assert_eq!(readback, payload);

    // The remainder of grain 0 (the 1024 bytes BEFORE the write) must
    // still match the original pattern.
    let head_off: u64 = 0;
    let head_len = grain_bytes - 1024;
    let mut head = vec![0u8; head_len];
    r.read_at(head_off, &mut head).unwrap();
    assert_eq!(head, pattern[..head_len]);

    drop(r);

    // GT entry for grain 1 was sparse and is now populated. Grain 0's
    // entry is unchanged.
    assert_eq!(
        read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 0),
        GRAIN0_OFF_SECTOR as u32
    );
    let g1 = read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 1);
    assert!(g1 >= (GRAIN0_OFF_SECTOR + GRAIN_SIZE) as u32);

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 5. write into a grain whose GT cluster isn't allocated
// ---------------------------------------------------------------------------

#[test]
fn write_into_grain_with_unallocated_gt_allocates_table_too() {
    let path = tmp_path("unallocated_gt");
    build_fully_sparse_two_gts(&path);

    // Pre-write: both GD entries are zero.
    assert_eq!(read_gd_entry(&path, 0), 0);
    assert_eq!(read_gd_entry(&path, 1), 0);

    // Pick a virtual offset inside GT slot 1: GT 0 covers
    // 512 * 64 KiB = 32 MiB of virtual address space. Anything past
    // that lands in GT 1.
    let virt: u64 = 32 * 1024 * 1024 + 4096; // 32 MiB + 4 KiB

    // open_rw needs the file to be writable. Use the public API.
    {
        let _ = OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("path must be writable");
    }
    let r = VmdkReader::open_rw(&path).unwrap();

    let payload = [0xABu8; 256];
    r.write_at(virt, &payload).unwrap();
    r.flush().unwrap();

    // Read back through the reader.
    let mut readback = [0u8; 256];
    r.read_at(virt, &mut readback).unwrap();
    assert_eq!(readback, payload);

    // A read inside GT 0's range still returns zeros — we only allocated
    // GT 1.
    let mut zeros = [0xFFu8; 64];
    r.read_at(0, &mut zeros).unwrap();
    assert!(zeros.iter().all(|&b| b == 0));

    drop(r);

    // GD slot 1 must now point at a real GT sector. GD slot 0 stays zero.
    assert_eq!(read_gd_entry(&path, 0), 0);
    let new_gt = read_gd_entry(&path, 1);
    assert!(new_gt > 0, "GT for slot 1 must be allocated");

    // The allocated GT's first entry should be 0; the entry for the
    // grain we wrote must point at a real grain sector.
    // virt = 32 MiB + 4 KiB → grain idx = (32 MiB + 4 KiB) / 64 KiB
    //   = (32*1024 + 4) / 64 = 524.something — wait, recompute:
    // grain_bytes = 64 KiB. virt = 33_558_528. virt / 64 KiB = 512.0625.
    // So this is grain index 512, gte_idx in GT 1 = 512 % 512 = 0.
    let new_grain_for_first_entry = read_grain_sector_value(&path, new_gt, 0);
    assert!(
        new_grain_for_first_entry > 0,
        "newly allocated GT entry [0] must point at the new grain"
    );

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Read-only sanity: open() rejects writes
// ---------------------------------------------------------------------------

#[test]
fn open_readonly_rejects_writes() {
    let path = tmp_path("ro_reject");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_grain0_only(&path, &pattern);

    let r = VmdkReader::open(&path).unwrap();
    let err = r.write_at(0, &[1u8; 4]);
    assert!(matches!(err, Err(vmdk::Error::ReadOnly)));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// open_rw_on_device refuses an RO inner device
// ---------------------------------------------------------------------------

#[test]
fn open_rw_on_device_refuses_readonly_inner() {
    let path = tmp_path("ro_inner");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_grain0_only(&path, &pattern);

    let dev = Arc::new(FileDevice::open(&path).unwrap()) as Arc<dyn BlockDevice>;
    let err = VmdkReader::open_rw_on_device(dev);
    assert!(matches!(err, Err(vmdk::Error::ReadOnly)));

    let _ = std::fs::remove_file(&path);
}
