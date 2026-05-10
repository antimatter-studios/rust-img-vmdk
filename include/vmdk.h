/*
 * am-img-vmdk C ABI — opens a VMDK (VMware Virtual Machine Disk) and
 * returns a generic FsCoreDevice handle. Once opened, all further
 * interaction goes through fs_core.h's device API.
 *
 * Link with libam_img_vmdk.a and include this header alongside fs_core.h.
 *
 * MIT license. (c) 2026 Antimatter Studios.
 */

#ifndef AM_IMG_VMDK_H
#define AM_IMG_VMDK_H

#include "fs_core.h"

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Open `path` (NUL-terminated UTF-8) as a VMDK image. Returns a generic
 * device handle; free via `fs_core_device_close`.
 *
 * On failure returns NULL and `fs_core_last_error_message()` has detail.
 *
 * Currently supported variants:
 *   - monolithicSparse (single file: header + descriptor + GD + GT + grains)
 *
 * Other VMDK variants (monolithicFlat split across files, twoGbMaxExtent,
 * streamOptimized, vmfs) currently return NULL with an "unsupported"
 * message; support may be added in future releases.
 *
 * `vmdk_open` opens read-only — `fs_core_device_write_at` returns
 * FS_CORE_READ_ONLY.
 *
 * `vmdk_open_rw` opens read-write. Writes succeed against allocated
 * grains (write-through) and against sparse grains (which trigger
 * grain allocation + grain-table mutation). Allocations land at the
 * device tail; crash-safety order is data → grain-table → grain
 * directory (when growing) → flush.
 */
FsCoreDevice *vmdk_open(const char *path);
FsCoreDevice *vmdk_open_rw(const char *path);

/*
 * Open a VMDK image whose backing storage is an existing FsCoreDevice
 * (e.g. an FSKit FSBlockDeviceResource lifted via
 * `fs_core_device_from_callbacks`, a slice reader, or any other
 * device the caller already holds). Use this when the VMDK layer
 * needs to sit on top of host-managed storage that isn't a path.
 *
 * Ownership: the returned handle takes over the input device. Do NOT
 * call `fs_core_device_close` on `inner` afterwards. On failure the
 * input is freed automatically and the function returns NULL.
 *
 * `vmdk_open_rw_on_device` requires the input device to report
 * `is_writable()`; otherwise it fails with FS_CORE_READ_ONLY.
 */
FsCoreDevice *vmdk_open_on_device(FsCoreDevice *inner);
FsCoreDevice *vmdk_open_rw_on_device(FsCoreDevice *inner);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* AM_IMG_VMDK_H */
