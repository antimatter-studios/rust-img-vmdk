#![no_main]
//! The sparse header: one sector carrying grain_size, num_gtes_per_gt,
//! gd_offset and capacity, every one of which is multiplied or divided
//! by on the read path.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = vmdk::SparseHeader::parse(data) {
        let _ = header.uses_zeroed_grain_marker();
        let _ = header.has_redundant_grain_directory();
        let _ = header.is_stream_optimized();
    }
});
