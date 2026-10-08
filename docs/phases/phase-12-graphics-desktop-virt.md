# Phase 12: Graphics, desktop, virtualization

Goal (milestone): in `cargo window`, a Wayland client draws a window that a MogOs compositor shows through
virtio-gpu, with keyboard and pointer input and sound; and MogOs hosts a MogOs guest as an ordinary process holding
vCPU handles. Graphics buffers are phase 6's memory objects and every wait is a completion, so there is no dma-buf,
no implicit sync and no separate event API.

## Steps

Done-whens name the e2e test, the benchmarks held and added, and the invariants (Step details). Benchmarks are hvf
medians of 21 interleaved boots against the step's base commit (`docs/BENCHMARKS.md`); "hold" means within noise, and
a slowdown is a failure, removed or shown unavoidable with numbers. Every device here is the virtio-mmio variant (`virtio-gpu-device`, `virtio-keyboard-device`, `virtio-tablet-device`, `virtio-sound-device`, `virtio-rng-device`, `virtio-serial-device`), so nothing waits on PCI or MSI-X; `cargo window`'s `virtio-tablet-pci` becomes `virtio-tablet-device`. Screens are checked headless: the e2e takes the
scanout through QEMU's monitor (`screendump`) and compares a hash or sampled pixels, and sends input with
`input-send-event`.

Needs: memory objects (phase 6), AF_UNIX with handle passing (phase 9 step 54c) and `poll` (phase 9 step 54a), hard-float C
(phase 9 step 53b), the firmware entry with its EL2 stub (phase 11 step
68) for 78. 74, 76, 77 and 78 are independent; 75 follows 74.

| # | Step | Done when |
| --- | --- | --- |
| 74 | virtio-gpu 2D and the display handle | A virtio-gpu driver (2D: resource create, attach backing, transfer to host, set scanout, flush). A display handle (rights: `commit`; init gets it) offers each scanout's modes and one atomic commit: a framebuffer memory object plus damage rectangles, all-or-nothing, completing as an `io_wait` op when the host has the frame (the commit is its own fence). Framebuffers are memory objects the client maps; the driver attaches their frames as backing once, so a commit copies nothing in the guest. e2e: `test=display` commits two frames with known patterns; `screendump` matches each; a commit with a framebuffer smaller than the mode is `EINVAL` and the screen is unchanged. |
| 75 | Input and a Wayland compositor | virtio-input (keyboard, tablet) events as stream handles. A compositor (`compositor`, native) speaks the Wayland wire protocol over AF_UNIX: `wl_compositor`, `wl_shm` (pools are memory objects passed as handles), `wl_seat` and `xdg_shell`; it composes damaged regions on the CPU into its framebuffer and commits through 74. A C client on libwayland-client (pinned, hard-float) opens a window and draws. e2e: the client's window appears at its position in `screendump`; a key and a pointer click sent through the monitor reach the focused client, which prints them. |
| 76 | Audio | A virtio-sound driver (PCM output and input streams) and a mixer service holding the device; clients get PCM stream handles whose writes are completion ops with the buffer charged to the client. e2e: with `-audiodev wav`, a client plays a known sine and the e2e reads the WAV file back: frequency and length match, no gap at the buffer boundaries. |
| 77 | virtio-rng and virtio-console | virtio-rng reseeds phase 9 step 54a's generator; virtio-console's ports are stream handles (a host channel for agents beside the UART). e2e: `random` output differs across boots with a fixed DT seed when virtio-rng is present; a line written by the host on a console port reaches a guest program and its answer comes back. |
| 78 | EL2 hypervisor | On a CPU with VHE (FEAT_VHE), step 68's entry keeps the kernel at EL2 with `HCR_EL2.E2H` set instead of dropping to EL1; without VHE it hosts no guests. A VM is a process holding a VM handle (guest RAM is a memory object mapped by stage-2 tables charged to its budget) and vCPU handles; running a vCPU is a completion op that returns an exit (MMIO, HVC/PSCI, WFI, a fault). The virtual GIC uses the GICv3 list registers; the guest timer is the virtual timer with its own offset. A user-space VMM (`vmm`, native) emulates virtio-mmio console and block. e2e on TCG `-M virt,virtualization=on -cpu max`: MogOs runs a MogOs guest from its `Image` (68); the guest prints its `boot:` line and passes `test=bench-syscall` and the shell test on its virtio disk. |

### Step details

- **74.** Benchmark (hvf): a full 1920x1080 frame and a 64x64 damaged region, commit to completion (median us).
  Invariants: a framebuffer is pinned for the device while a commit using it is in flight (the memory object's
  frames cannot be unmapped from the client until it completes); only the display handle's holder commits. Avoids:
  per-driver uAPIs (M2), implicit sync, and a separate buffer-sharing object (dma-buf).
- **75.** Benchmark: input event to client (median us); frame time of composing one moving window. Invariants: a
  client sees input only while focused; a client's buffer is read only between its commit and the release event.
  Avoids: X11's global input and window authority; a kernel-side window system.
- **76.** Benchmark: output latency (submit to device consumption) at 10 ms and 2 ms buffers. Invariants: a
  stream's buffer is charged to its client; an underrun is reported, never filled with stale data. Avoids: a kernel
  mixer.
- **77.** Invariants: entropy from the device only adds to the generator's state, never replaces it.
- **78.** Benchmark: guest syscall and yield against native (no exits on either path); exit round trip for an MMIO
  access and an HVC (median ns) on TCG, and under hvf once the host's QEMU runs nested EL2 (see Notes). Invariants:
  a VM reaches only its memory object's frames (stage 2), and its budget bounds every guest page and table; a vCPU
  handle without the `run` right cannot enter the guest. Avoids: a kernel-resident device model (KVM plus QEMU's
  split is kept, with the VMM unprivileged and holding only the VM's handles).

## Decided

- **Display: one atomic commit per scanout with the commit as its own fence** (coordinator, 2026-10-07): the part of
  Linux's DRM/KMS worth copying is atomic modesetting and explicit fences (docs.kernel.org/gpu/drm-kms.html, from the
  survey); the per-driver ioctl surface and implicit sync are not.
- **The compositor speaks Wayland**: clients bring their own buffers and the protocol is a byte stream with handle
  passing, which MogOs already has; it is what Linux GUI programs under the phase 9 compat layer expect. The protocol
  description is general knowledge (wayland.freedesktop.org, not re-read this session).
- **3D is not scheduled**: QEMU 9.2.1 on this host has no `virtio-gpu-gl` device (checked with `-device help`), and
  upstream QEMU's macOS path for Venus was still an RFC in December 2025 (patchew.org, the UTM author's series). When
  a host offers it, Venus (Vulkan over virtio-gpu) is the path: QEMU documents virgl (OpenGL), Venus and DRM native
  contexts (qemu.org/docs/master/system/devices/virtio/virtio-gpu.html), and Vulkan is the API new GPU stacks build
  on.
- **Hypervisor: VHE only** (step 78): running the hypervisor and its host kernel at EL2 is what "Optimizing the
  Design and Implementation of the Linux ARM Hypervisor" (Dall, Li, Nieh, USENIX ATC 2017) reports as an order of
  magnitude improvement for KVM/ARM transitions (abstract; the body was not re-read), and it keeps one kernel path.
  Hosting on cores without VHE (A72) is not supported; guests on them run fine.

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12): 74-76 and 78 keep their numbers; 77
  keeps virtio-rng and the console only. Not scheduled: vsock (QEMU 9.2.1 here has no vsock device, so it cannot be
  tested here, and needs a Linux host's vhost-vsock), the balloon (it exists for overcommit, which MogOs does not
  do), and survey 79's
  live update (drivers are in the kernel, ROADMAP Decided, and the user-space servers are few).
- hvf nested virtualization: Apple exposes EL2 to guests on M3 and later with macOS 15 and later, and QEMU 11.1
  (August 2026) supports it on `virt`; both from secondary sources (linuxiac.com, search snippets), and Apple's
  implementation is reported to be nVHE-only for the guest's EL2, which 78 needs VHE in. Not verified on this host
  (QEMU 9.2.1, M4 Pro); until it is, 78's numbers are TCG's.

## What was done

Filled in as each step lands.
