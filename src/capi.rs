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
    open_path(path, false)
}

/// Open `path` read-write. Writes succeed against allocated grains
/// (write-through) and against sparse grains (which trigger grain
/// allocation + grain-table mutation). Other VMDK variants stay
/// rejected at open time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmdk_open_rw(path: *const c_char) -> *mut FsCoreDevice {
    open_path(path, true)
}

/// Open a VMDK image whose backing storage is an existing
/// [`FsCoreDevice`] handle. Use this when the caller already holds the
/// device (e.g. an FSKit `FSBlockDeviceResource` lifted into an
/// `FsCoreDevice` via `fs_core_device_from_callbacks`) and wants the
/// VMDK layer to sit on top of it.
///
/// Takes ownership of the input `inner` handle on success — the caller
/// must NOT call `fs_core_device_close` on it afterwards. On failure the
/// input is freed automatically and the function returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmdk_open_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice {
    unsafe { open_on_device(inner, false) }
}

/// Read-write variant of [`vmdk_open_on_device`]. The input device must
/// report `is_writable()`; otherwise the open fails with
/// `FS_CORE_READ_ONLY` and the input is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmdk_open_rw_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice {
    unsafe { open_on_device(inner, true) }
}

unsafe fn open_on_device(inner: *mut FsCoreDevice, writable: bool) -> *mut FsCoreDevice {
    if inner.is_null() {
        set_last_error("inner device handle is null");
        return ptr::null_mut();
    }
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // Reclaim ownership of the boxed handle; Arc::clone the inner
        // device so we can stack it under the VMDK reader. The original
        // handle box is dropped at the end of this scope (releasing the
        // FsCoreDevice wrapper), but the underlying Arc<dyn BlockDevice>
        // lives on inside the new VmdkReader.
        let boxed = unsafe { Box::from_raw(inner) };
        let dev_arc = boxed.inner().clone();
        drop(boxed);

        let reader = if writable {
            VmdkReader::open_rw_on_device(dev_arc)
        } else {
            VmdkReader::open_on_device(dev_arc)
        };
        match reader {
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
            set_last_error("panic in vmdk_open_on_device");
            ptr::null_mut()
        }
    }
}

fn open_path(path: *const c_char, writable: bool) -> *mut FsCoreDevice {
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
        let reader = if writable {
            VmdkReader::open_rw(s)
        } else {
            VmdkReader::open(s)
        };
        match reader {
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
