//! C ABI for the VMDK reader. Returns a generic [`FsCoreDevice`] handle
//! so consumers route through the same opaque-handle convention every
//! sister crate uses.

#![allow(clippy::missing_safety_doc)]

use crate::VmdkReader;
use fs_core::ffi::{set_last_error, FsCoreDevice};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::Arc;

/// Open `path` (NUL-terminated UTF-8) as a VMDK image and return a
/// generic device handle. On failure returns NULL; consult
/// `fs_core_last_error_message()` for detail.
///
/// Currently only the `monolithicSparse` variant is supported. Other
/// variants (`monolithicFlat`, `twoGbMaxExtent*`, `streamOptimized`,
/// `vmfs*`) return NULL with an "unsupported" message.
///
/// Read-only — `fs_core_device_write_at` returns `FS_CORE_READ_ONLY`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmdk_open(path: *const c_char) -> *mut FsCoreDevice {
    if path.is_null() {
        set_last_error("path is null");
        return ptr::null_mut();
    }
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let cstr = unsafe { CStr::from_ptr(path) };
        let s = match cstr.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error("path is not valid UTF-8");
                return ptr::null_mut();
            }
        };
        match VmdkReader::open(s) {
            Ok(r) => FsCoreDevice::into_handle(Arc::new(r)),
            Err(e) => {
                set_last_error(e.to_string());
                ptr::null_mut()
            }
        }
    }));
    match res {
        Ok(p) => p,
        Err(_) => {
            set_last_error("panic in vmdk_open");
            ptr::null_mut()
        }
    }
}
