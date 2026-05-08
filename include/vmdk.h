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
 * Read-only — `fs_core_device_write_at` returns FS_CORE_READ_ONLY.
 */
FsCoreDevice *vmdk_open(const char *path);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* AM_IMG_VMDK_H */
