# Phase 4: I/O and Storage

Goal: talk to devices, read and write files, and interact through a shell.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 15 | Driver model + virtio | virtio-blk reads a sector from a QEMU disk image. |
| 16 | VFS + ramfs/initramfs | Files from an archive bundled with the kernel can be opened and read. |
| 17 | Disk filesystem | FAT32 (or our own format) mounted from virtio-blk. |
| 18 | Shell | A user-space shell over the UART runs commands. |

## Notes

- Filesystem placement (in kernel vs user-space server) follows the kernel-architecture decision.
- Page cache comes after the heap and VFS are stable.
