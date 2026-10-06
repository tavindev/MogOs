# Phase 4: Shell and Storage

Goal: an interactive POSIX shell first, then storage with a checksummed copy-on-write filesystem.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 17 | File objects over the boot archive | The kernel VFS is a file-object interface (lookup, read, readdir) with the cpio archive behind it; `/` is just the process's root directory handle (held by libc). `..` and absolute paths cannot escape a directory handle (e2e). |
| 18 | Console input | GIC SPI enabled for the UART; UART receive with a minimal line discipline (echo, line editing). The e2e harness writes to QEMU's stdin and asserts the echoed output. |
| 19 | musl + shell | musl's `syscall_arch.h` replaced by a dispatcher from Linux syscall numbers to native calls (anything else returns `-ENOSYS`); the fd table is a libc map over handles; stdin/stdout/stderr are fixed handle slots at spawn. `posix_spawn`/`vfork` map to native spawn; `fork` returns `ENOSYS` for now. Signals: `^C` kills the shell's foreground child through its process handle; `SIGCHLD` comes through `wait`; others are ignored. busybox (no-fork configuration, to verify) runs `sh`, `ls`, `cat` from the archive. Needs a C cross toolchain on macOS. |
| 20 | virtio-blk | virtio-blk over virtio-mmio (found in the DTB; pin legacy vs modern after checking QEMU's default) reads, writes and flushes sectors through the step 15 completion path. Throughput benchmark recorded. |
| 21a | MogFS format + read-only mount | An on-disk format crate shared by the kernel and a host image tool; mount verifies checksums on all data and metadata. e2e: a corrupted block reads as `EIO`. |
| 21b | MogFS copy-on-write writes | Writes go to new blocks; a commit flushes, writes the root, flushes. |
| 21c | MogFS atomic commits | Everything written between two explicit commits lands atomically (group commit, no isolation). e2e: `test=crash-after=N` powers off before the root write; a second boot on the same image sees the old state intact. |

## Notes

- The filesystem lives in the kernel (monolithic), behind the file-object interface.
- A shared-memory submission ring is a later optimization of step 15's completion path, only if its benchmark justifies it.
- Linux binary compatibility comes after this phase (see ROADMAP).

## What was done

Filled in as each step lands.
