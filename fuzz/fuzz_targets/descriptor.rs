#![no_main]
//! The embedded text descriptor.
//!
//! A different shape of hazard from the binary header: unbounded line
//! lengths, extent lines whose declared sizes need not match the file,
//! and parent links. `from_utf8` rather than a lossy conversion,
//! because that is what the read path does -- a descriptor that is not
//! UTF-8 is refused there, so fuzzing a lossily-repaired one would test
//! a path no image can reach.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = vmdk::descriptor::Descriptor::parse(text);
});
