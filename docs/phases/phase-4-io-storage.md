# Phase 4: I/O and Storage

Goal: talk to devices, read and write files, and interact through a shell.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 15 | Driver model + virtio | virtio-blk reads a sector from a QEMU disk image. |
| 16 | VFS + ramfs/initramfs | Files from an archive bundled with the kernel can be opened and read. |
| 17 | Disk filesystem | FAT32 (or our own format) mounted from virtio-blk. |
| 18 | libc + shell | A libc (relibc or musl) ported to the native ABI; busybox `sh` and core utilities run over the UART. |

## Notes

- Filesystem lives in the kernel (monolithic), behind a VFS with POSIX semantics.
- Page cache comes after the heap and VFS are stable.
