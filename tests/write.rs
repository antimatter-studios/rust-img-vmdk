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
use std::sync::Arc;

use fs_core::{BlockDevice, BlockRead, FileDevice};
use vmdk::header::{
    offsets, FLAG_REDUNDANT_GRAIN_TABLE, FLAG_ZEROED_GRAIN, GTE_ZEROED_GRAIN, HEADER_SIZE, MAGIC,
};
use vmdk::VmdkReader;

mod common;
use common::{patch, TempPath};

/// Self-deleting, so a panicking assertion leaves nothing behind.
fn tmp_path(name: &str) -> TempPath {
    TempPath::new(&format!("write_{name}"))
}

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
fn build_grain0_only(path: &std::path::Path, grain0_pattern: &[u8]) {
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
fn build_fully_sparse_two_gts(path: &std::path::Path) {
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

/// Layout for a fixture that carries a **redundant** grain directory,
/// the way every sparse VMDK qemu and VMware produce does.
///
/// The redundant copy comes first in the file, mirroring the order those
/// tools use: redundant directory, redundant table, primary directory,
/// primary table, then the grains.
const RGD_OFF_SECTOR: u64 = 2;
const RGT_OFF_SECTOR: u64 = 3; // sectors 3..6
const RGD_GD_OFF_SECTOR: u64 = 7;
const RGD_GT_OFF_SECTOR: u64 = 8; // sectors 8..11
const RGD_GRAIN0_OFF_SECTOR: u64 = 12;

/// Build a 1 MiB sparse VMDK with both grain directories present, no
/// grains allocated, and `flags` declaring the redundant copy live.
fn build_with_redundant_directory(path: &std::path::Path) {
    let mut h = build_header();
    h[8..12].copy_from_slice(&FLAG_REDUNDANT_GRAIN_TABLE.to_le_bytes());
    h[48..56].copy_from_slice(&RGD_OFF_SECTOR.to_le_bytes());
    h[56..64].copy_from_slice(&RGD_GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&RGD_GRAIN0_OFF_SECTOR.to_le_bytes());

    let mut rgd_sector = [0u8; SECTOR as usize];
    rgd_sector[0..4].copy_from_slice(&(RGT_OFF_SECTOR as u32).to_le_bytes());
    let mut gd_sector = [0u8; SECTOR as usize];
    gd_sector[0..4].copy_from_slice(&(RGD_GT_OFF_SECTOR as u32).to_le_bytes());

    let gt_bytes = vec![0u8; (NUM_GTES_PER_GT as usize) * 4];

    let mut f = File::create(path).unwrap();
    f.set_len(RGD_GRAIN0_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&h, 0).unwrap();
    f.write_all_at(&build_descriptor_sector(), DESC_OFF_SECTOR * SECTOR)
        .unwrap();
    f.write_all_at(&rgd_sector, RGD_OFF_SECTOR * SECTOR)
        .unwrap();
    f.write_all_at(&gt_bytes, RGT_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&gd_sector, RGD_GD_OFF_SECTOR * SECTOR)
        .unwrap();
    f.write_all_at(&gt_bytes, RGD_GT_OFF_SECTOR * SECTOR)
        .unwrap();
}

/// Read a whole grain table as sector numbers.
fn read_grain_table(path: &std::path::Path, gt_sector: u32) -> Vec<u32> {
    let mut f = File::open(path).unwrap();
    f.seek(SeekFrom::Start((gt_sector as u64) * SECTOR))
        .unwrap();
    let mut bytes = vec![0u8; (NUM_GTES_PER_GT as usize) * 4];
    f.read_exact(&mut bytes).unwrap();
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Report the first grain-table entry on which two copies disagree.
/// `assert_eq!` on 512-entry tables prints both in full, which buries
/// the one number that differs.
fn assert_tables_agree(primary: &[u32], redundant: &[u32]) {
    assert_eq!(primary.len(), redundant.len(), "grain table lengths differ");
    if let Some(i) = primary.iter().zip(redundant).position(|(a, b)| a != b) {
        panic!(
            "grain table entry {i} is {} in the primary copy and {} in the redundant one",
            primary[i], redundant[i]
        );
    }
}

fn read_directory_entry(path: &std::path::Path, dir_sector: u64, idx: u64) -> u32 {
    let mut f = File::open(path).unwrap();
    f.seek(SeekFrom::Start(dir_sector * SECTOR + idx * 4))
        .unwrap();
    let mut bytes = [0u8; 4];
    f.read_exact(&mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn read_grain_sector_value(path: &std::path::Path, gt_sector: u32, gte_idx: u64) -> u32 {
    let mut f = File::open(path).unwrap();
    let off = (gt_sector as u64) * SECTOR + gte_idx * 4;
    f.seek(SeekFrom::Start(off)).unwrap();
    let mut bytes = [0u8; 4];
    f.read_exact(&mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn read_gd_entry(path: &std::path::Path, idx: u64) -> u32 {
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
// 6. write into a grain carrying the zeroed-grain marker
// ---------------------------------------------------------------------------

/// A grain-table entry of `1` announces "present and entirely zero". It
/// is a sentinel, not a sector number, and the sector it would name — 1 —
/// holds the embedded descriptor.
///
/// So a write into such a grain that mistakes the sentinel for a pointer
/// does not merely put the payload in the wrong place: it puts it on top
/// of the descriptor, and the image stops being a VMDK. The grain must be
/// allocated instead, exactly as an entry of `0` would be.
#[test]
fn write_into_a_zeroed_grain_allocates_rather_than_overwriting_the_descriptor() {
    let path = tmp_path("zg_write");
    let pattern: Vec<u8> = (0u8..=255u8)
        .cycle()
        .take((GRAIN_SIZE * SECTOR) as usize)
        .collect();
    build_grain0_only(&path, &pattern);
    patch(
        &path,
        offsets::FLAGS as u64,
        &FLAG_ZEROED_GRAIN.to_le_bytes(),
    );
    patch(
        &path,
        GT_OFF_SECTOR * SECTOR + 4,
        &GTE_ZEROED_GRAIN.to_le_bytes(),
    );

    // What the descriptor sector holds before the write.
    let descriptor_before = read_sector(&path, DESC_OFF_SECTOR);
    assert!(
        descriptor_before.starts_with(b"# Disk DescriptorFile"),
        "fixture precondition: sector 1 is the descriptor"
    );

    let payload = [0x5Au8; 4096];
    let r = VmdkReader::open_rw(&path).unwrap();
    r.write_at(GRAIN_SIZE * SECTOR, &payload).unwrap();
    r.flush().unwrap();

    let mut readback = [0u8; 4096];
    r.read_at(GRAIN_SIZE * SECTOR, &mut readback).unwrap();
    assert_eq!(readback, payload, "the write must be readable back");

    // The rest of the grain still reads as zero: allocating over a
    // zeroed-grain marker has to preserve what the marker promised.
    let mut tail = [0xFFu8; 512];
    r.read_at(GRAIN_SIZE * SECTOR + 4096, &mut tail).unwrap();
    assert!(tail.iter().all(|&b| b == 0));
    drop(r);

    let descriptor_after = read_sector(&path, DESC_OFF_SECTOR);
    assert_eq!(
        without_content_id(&descriptor_before),
        without_content_id(&descriptor_after),
        "the write landed on the embedded descriptor at sector 1"
    );
    // The one part of the descriptor a write is *supposed* to change.
    assert_ne!(
        content_id(&descriptor_before),
        content_id(&descriptor_after),
        "a write must change the content identifier"
    );

    // The grain-table entry must now name a real grain, past the metadata.
    let entry = read_grain_sector_value(&path, GT_OFF_SECTOR as u32, 1);
    assert!(
        entry > GRAIN0_OFF_SECTOR as u32,
        "the marker must be replaced by a freshly allocated grain, got {entry}"
    );

    let _ = std::fs::remove_file(&path);
}

/// Where the eight hex digits of the descriptor's `CID=` value sit.
fn content_id_at(region: &[u8]) -> usize {
    let text = String::from_utf8_lossy(region);
    text.find("\nCID=")
        .expect("the fixture descriptor has a CID")
        + 5
}

fn content_id(region: &[u8]) -> Vec<u8> {
    let at = content_id_at(region);
    region[at..at + 8].to_vec()
}

/// The descriptor with its content identifier blanked, so two of them
/// can be compared for everything a write must *not* change.
fn without_content_id(region: &[u8]) -> Vec<u8> {
    let mut out = region.to_vec();
    let at = content_id_at(region);
    out[at..at + 8].fill(b'-');
    out
}

fn read_sector(path: &std::path::Path, sector: u64) -> Vec<u8> {
    let mut f = File::open(path).unwrap();
    f.seek(SeekFrom::Start(sector * SECTOR)).unwrap();
    let mut buf = vec![0u8; SECTOR as usize];
    f.read_exact(&mut buf).unwrap();
    buf
}

// ---------------------------------------------------------------------------
// 7. concurrent writers
// ---------------------------------------------------------------------------

/// `VmdkReader` takes `&self` for writes, is `Send + Sync`, and the C
/// ABI hands it out behind an `Arc` — so any thread holding the handle
/// can write. Several threads writing into the *same* not-yet-allocated
/// grain table must therefore all keep their data.
///
/// The failure this pins returns `Ok(())` to every writer and loses one
/// of them: two threads both read a grain-directory entry of 0, both
/// allocate a grain table, the second overwrites the first's directory
/// slot, and the first then publishes its grain pointer into a table the
/// directory no longer references. The bytes are on disk, in a grain
/// nothing links, and the offset reads back as zeros.
///
/// Reopening from scratch is what makes the assertion honest: a stale
/// in-memory grain-table cache can otherwise report the pointer that the
/// file does not actually carry.
#[test]
fn concurrent_writers_into_one_unallocated_grain_table_all_survive() {
    const WRITERS: u64 = 8;
    const ROUNDS: usize = 4;

    for round in 0..ROUNDS {
        let path = tmp_path(&format!("concurrent_{round}"));
        build_fully_sparse_two_gts(&path);

        let r = Arc::new(VmdkReader::open_rw(&path).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(WRITERS as usize));
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let r = Arc::clone(&r);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                // Distinct grains, all inside grain table 0.
                let offset = w * GRAIN_SIZE * SECTOR;
                let payload = [0xA0u8 + w as u8; 512];
                barrier.wait();
                r.write_at(offset, &payload)
                    .expect("a concurrent write must not error");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        r.flush().unwrap();
        drop(r);

        // Reopen: only what the file itself links counts.
        let r = VmdkReader::open(&path).unwrap();
        for w in 0..WRITERS {
            let mut got = [0u8; 512];
            r.read_at(w * GRAIN_SIZE * SECTOR, &mut got).unwrap();
            let want = 0xA0u8 + w as u8;
            assert!(
                got.iter().all(|&b| b == want),
                "round {round}: writer {w}'s data was accepted and then lost \
                 (wanted {want:#x} throughout, first byte back was {:#x})",
                got[0]
            );
        }
        drop(r);
        let _ = std::fs::remove_file(&path);
    }
}

/// The same race one level down: several threads writing into distinct
/// parts of the *same* grain, whose grain table already exists. Each
/// sees an entry of 0, each allocates its own grain, and the last
/// `update_gt_entry` wins — orphaning the others' payloads while
/// returning `Ok(())` to all of them.
#[test]
fn concurrent_writers_into_one_sparse_grain_all_survive() {
    const WRITERS: u64 = 8;
    const ROUNDS: usize = 4;
    const CHUNK: u64 = 512;

    for round in 0..ROUNDS {
        let path = tmp_path(&format!("concurrent_grain_{round}"));
        let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
        // Grain 0 is allocated, so grain table 0 exists; grain 1 is
        // sparse and is what every writer lands in.
        build_grain0_only(&path, &pattern);

        let r = Arc::new(VmdkReader::open_rw(&path).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(WRITERS as usize));
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let r = Arc::clone(&r);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let offset = GRAIN_SIZE * SECTOR + w * CHUNK;
                let payload = vec![0xB0u8 + w as u8; CHUNK as usize];
                barrier.wait();
                r.write_at(offset, &payload)
                    .expect("a concurrent write must not error");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        r.flush().unwrap();
        drop(r);

        let r = VmdkReader::open(&path).unwrap();
        for w in 0..WRITERS {
            let mut got = vec![0u8; CHUNK as usize];
            r.read_at(GRAIN_SIZE * SECTOR + w * CHUNK, &mut got)
                .unwrap();
            let want = 0xB0u8 + w as u8;
            assert!(
                got.iter().all(|&b| b == want),
                "round {round}: writer {w}'s data was accepted and then lost \
                 (wanted {want:#x} throughout, first byte back was {:#x})",
                got[0]
            );
        }
        drop(r);
        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// 8. the redundant grain directory
// ---------------------------------------------------------------------------

/// A sparse VMDK carries a second copy of the grain directory and of
/// every grain table, and every image qemu or VMware produces declares
/// it live. It exists to be read when the primary is damaged, so a write
/// that updates only the primary leaves it claiming a grain is absent
/// when the data is on disk — and the recovery it exists for then
/// returns a hole.
///
/// Nothing catches that at the time: `qemu-img check` does not consult
/// the redundant tables, so the image passes.
#[test]
fn a_write_updates_both_copies_of_the_grain_table() {
    let path = tmp_path("redundant_dirs");
    build_with_redundant_directory(&path);

    let payload = [0x7Eu8; 4096];
    let r = VmdkReader::open_rw(&path).unwrap();
    // Grain 3 — sparse, in a grain table that already exists in both
    // directories.
    r.write_at(3 * GRAIN_SIZE * SECTOR, &payload).unwrap();
    r.flush().unwrap();
    drop(r);

    let primary = read_grain_table(&path, RGD_GT_OFF_SECTOR as u32);
    let redundant = read_grain_table(&path, RGT_OFF_SECTOR as u32);
    assert!(primary[3] != 0, "the primary table must name the new grain");
    assert_tables_agree(&primary, &redundant);

    // And the data is still readable, i.e. the mirror did not scribble
    // over the grain it was describing.
    let r = VmdkReader::open(&path).unwrap();
    let mut got = [0u8; 4096];
    r.read_at(3 * GRAIN_SIZE * SECTOR, &mut got).unwrap();
    assert_eq!(got, payload);

    let _ = std::fs::remove_file(&path);
}

/// A grain table this crate allocates has to appear in *both*
/// directories. Publishing it into the primary alone leaves the
/// redundant directory with no table at all for that slot, which is
/// worse than a stale entry: every grain in it is invisible to a
/// fallback read, not just the one just written.
#[test]
fn an_allocated_grain_table_appears_in_both_directories() {
    let path = tmp_path("redundant_alloc_gt");
    build_with_redundant_directory(&path);
    // Empty both directory slots so the write has to allocate a table.
    let zeroed = [0u8; 4];
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.write_all_at(&zeroed, RGD_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(&zeroed, RGD_GD_OFF_SECTOR * SECTOR).unwrap();
    drop(f);

    let payload = [0x5Cu8; 512];
    let r = VmdkReader::open_rw(&path).unwrap();
    r.write_at(0, &payload).unwrap();
    r.flush().unwrap();
    drop(r);

    let primary_gt = read_directory_entry(&path, RGD_GD_OFF_SECTOR, 0);
    let redundant_gt = read_directory_entry(&path, RGD_OFF_SECTOR, 0);
    assert!(primary_gt != 0, "the primary directory must name a table");
    assert!(
        redundant_gt != 0,
        "the redundant directory has no grain table for a slot the primary does"
    );
    assert_ne!(
        primary_gt, redundant_gt,
        "the two copies must be distinct tables, not the same one twice"
    );
    assert_tables_agree(
        &read_grain_table(&path, primary_gt),
        &read_grain_table(&path, redundant_gt),
    );

    let _ = std::fs::remove_file(&path);
}

/// The redundant directory is not consulted on the read path, so an
/// image whose redundant directory does not fit the file is perfectly
/// readable and opens read-only. Opening it for *writing* is what cannot
/// be honoured: the copy a recovery tool falls back to would be left
/// wrong, and nothing downstream would notice.
#[test]
fn an_unreachable_redundant_directory_reads_but_refuses_to_open_rw() {
    let path = tmp_path("redundant_past_eof");
    build_with_redundant_directory(&path);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.write_all_at(&1_000_000u64.to_le_bytes(), 48).unwrap();
    drop(f);

    VmdkReader::open(&path).expect("a read-only open does not need the redundant copy");
    let err = VmdkReader::open_rw(&path)
        .err()
        .expect("expected a refusal, got Ok");
    assert!(
        format!("{err}").contains("redundant grain directory"),
        "the refusal must name the redundant directory, got {err}"
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

// ---------------------------------------------------------------------------
// 9. the unclean-shutdown marker
// ---------------------------------------------------------------------------

/// `uncleanShutdown` records that a writer had the image open and did
/// not close it properly. It was parsed into the header struct and never
/// set, never cleared, and never consulted — so an image this crate
/// crashed halfway through was byte-indistinguishable from one it closed
/// cleanly, and nothing downstream could even know to look. The module
/// doc already admits a crash mid-allocation may leak a grain.
///
/// It is raised on the first write rather than at open, so that opening
/// an image read-write and only reading from it does not modify the
/// file, and lowered on a clean `flush`, which is the caller saying its
/// writes are complete.
#[test]
fn the_unclean_shutdown_marker_stands_between_a_write_and_a_flush() {
    let path = tmp_path("unclean");
    let pattern = vec![0u8; (GRAIN_SIZE * SECTOR) as usize];
    build_grain0_only(&path, &pattern);
    assert_eq!(unclean_byte(&path), 0, "fixture precondition");

    let r = VmdkReader::open_rw(&path).unwrap();
    let mut probe = [0u8; 16];
    r.read_at(0, &mut probe).unwrap();
    assert_eq!(
        unclean_byte(&path),
        0,
        "opening read-write and only reading must not modify the image"
    );

    r.write_at(0, &[0x11u8; 512]).unwrap();
    assert_eq!(
        unclean_byte(&path),
        1,
        "a write with no flush behind it must leave the marker standing"
    );

    r.flush().unwrap();
    assert_eq!(
        unclean_byte(&path),
        0,
        "a clean flush must lower the marker"
    );

    // And it goes back up for the next write.
    r.write_at(1024, &[0x22u8; 512]).unwrap();
    assert_eq!(unclean_byte(&path), 1);
    r.flush().unwrap();
    assert_eq!(unclean_byte(&path), 0);
    drop(r);

    // The image is still readable, and the header still parses.
    let r = VmdkReader::open(&path).unwrap();
    assert_eq!(r.header().unclean_shutdown, 0);
    let mut got = [0u8; 512];
    r.read_at(0, &mut got).unwrap();
    assert_eq!(got, [0x11u8; 512]);

    let _ = std::fs::remove_file(&path);
}

fn unclean_byte(path: &std::path::Path) -> u8 {
    let mut f = File::open(path).unwrap();
    f.seek(SeekFrom::Start(offsets::UNCLEAN_SHUTDOWN as u64))
        .unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0]
}
