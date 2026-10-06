# Phase 4: I/O, Shell, and Storage

Goal: an interactive POSIX shell first, then fast asynchronous storage with a checksummed copy-on-write filesystem.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 17 | VFS over the boot archive | A VFS with POSIX semantics mounts the phase 3 cpio archive at `/`; directory handles resolve paths; a program opens and reads a file. |
| 18 | Console input + terminal | UART receive with a minimal line discipline (echo, line editing, `^C`). The e2e harness writes to QEMU's stdin and asserts the echoed output. |
| 19 | musl + shell | musl ported to the native ABI (its syscall layer mapped to native calls, with `fork` and signals emulated); busybox `sh`, `ls`, `cat` run from the archive over the UART. |
| 20 | Completion-based I/O + virtio-blk | A submission/completion ring per process is the native I/O interface (no blocking-read-only path). virtio-blk over virtio-mmio (found in the DTB) reads and writes sectors through it. Ring throughput benchmark recorded. |
| 21 | MogFS | Copy-on-write filesystem with checksums on all data and metadata, and atomic multi-file transactions. A host tool builds images. e2e: corrupting a block on disk is detected on read, and a transaction interrupted by power-off leaves the old state intact. |

## Notes

- The filesystem lives in the kernel (monolithic), behind the VFS.
- Linux binary compatibility comes after this phase (see ROADMAP).

## What was done

Filled in as each step lands.
