# Phase 4: Shell and Storage

Goal (milestone): from a shell on the console, `ls`, `mkdir`, create a file, write to it, and save; after a reboot on the same disk image, the directory and file contents are still there. POSIX userland (musl, busybox) follows the milestone.

## Steps

Steps 18, 20 and 21 are independent and run in parallel; 22 integrates them.

| # | Step | Done when |
| --- | --- | --- |
| 18 | Console input | GIC SPI enabled for the UART; UART receive with a minimal line discipline (echo, backspace, line delivered on Enter). A blocking `io_submit_wait` read on the console handle returns one line. The e2e harness writes to QEMU's stdin and asserts the echoed line and what the program read. |
| 20 | virtio-blk | virtio-blk over virtio-mmio (found in the DTB; legacy vs modern pinned after checking QEMU's default) reads, writes and flushes 4 KiB blocks. Kernel-internal block API (no user handle yet). e2e: boot with a disk image, write a block, flush, reboot, read it back. Throughput benchmark recorded. |
| 21 | MogFS crate | A safe, `no_std` crate over a `Disk` trait (read/write/flush 4 KiB blocks), shared by the kernel and a host `mkfs`. Copy-on-write: every change writes new blocks; `commit` flushes, writes the next superblock (two alternating slots, generation + checksum), flushes. Checksums on every block; a bad block reads as an error. Operations: lookup, readdir, mkdir, create, read, write (whole-file or append is enough), commit. Host-tested with an in-memory disk: round trip, corruption detected, power cut at every write of a commit leaves the old or the new state, never a mix. |
| 22 | Files + native shell (milestone) | Kernel file objects with MogFS and the boot archive behind one interface: `open(dir, name, flags)` (create), `mkdir(dir, name)`, `readdir`, read/write through `io_submit_wait`, `sync` (commit). `..` and absolute paths cannot escape a directory handle. A tiny native shell (`msh`, `crates/user`) with `ls`, `mkdir`, `touch`, `write <file> <text>`, `cat`, `sync`. e2e: boot 1 runs `mkdir docs`, `write docs/a.txt hello`, `sync`; boot 2 on the same image runs `ls docs` and `cat docs/a.txt` and sees `a.txt` and `hello`. |
| 23 | musl + busybox | musl's `syscall_arch.h` replaced by a dispatcher from Linux syscall numbers to native calls (anything else returns `-ENOSYS`); the fd table is a libc map over handles; stdin/stdout/stderr are fixed handle slots at spawn. `posix_spawn`/`vfork` map to native spawn; `fork` returns `ENOSYS` for now. Signals: `^C` kills the foreground child through its process handle; `SIGCHLD` comes through `wait`; others are ignored. busybox (no-fork configuration, to verify) runs `sh`, `ls`, `cat`. Needs a C cross toolchain on macOS. |

## Notes

- The filesystem lives in the kernel (monolithic), behind the file-object interface. MogFS logic is a pure crate so it is host-tested without QEMU.
- `msh` exists to reach the milestone without a libc port; busybox replaces it as the default shell in step 23.
- A shared-memory submission ring is a later optimization of step 15's completion path, only if its benchmark justifies it.
- Linux binary compatibility comes after this phase (see ROADMAP).

## What was done

Filled in as each step lands.
