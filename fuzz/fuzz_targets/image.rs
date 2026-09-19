#![no_main]
//! A whole image, opened and read.
//!
//! The header declares where the descriptor and the grain directory
//! live; the descriptor declares the extents; the grain directory
//! points at grain tables which point at grains. Only opening an image
//! reaches all four.
use libfuzzer_sys::fuzz_target;
use vmdk_fuzz::walk;

fuzz_target!(|data: &[u8]| {
    walk(data);
});
