# Phase 11: Real hardware, boot, power

Goal (milestone): MogOs boots from UEFI firmware on a Radxa Orion O6 (Decided) to msh with console, NVMe storage and
network, runs the cross-OS benchmarks there against Linux on the same board, and idles and suspends without a
periodic tick. QEMU stays the gate: every step has a QEMU e2e (`gic-version=3,its=on`, edk2 as firmware,
`iommu=smmuv3`, `qemu-xhci`); board runs are scripted over its serial console and recorded in What was done.

## Steps

Done-whens name the e2e test, the benchmarks held and added, and the invariants (Step details). Benchmarks are hvf
medians of 21 interleaved boots against the step's base commit (`docs/BENCHMARKS.md`): yield, syscall, pipe and boot
at `-smp 1` and `-smp 4`, plus the block and network rows a step names; "hold" means within noise, and a slowdown is a
failure, removed or shown unavoidable with numbers.

Needs: phase 6's higher-half, relocatable kernel (68 loads at whatever address firmware picks), phase 7 step 45 (PCIe,
NVMe and a minimal ITS), phase 9 step 53a (the `tty` service, for 70). 67 and 68 run in parallel; 68a follows 68 (the
Orion O6's firmware is expected to hand over ACPI); 69 and 70 follow 67; 71 needs 67, 68 and 68a; 71a follows 71; 72
can run any time after phase 5; 73 follows 72.

| # | Step | Done when |
| --- | --- | --- |
| 67 | ITS at board scale | Phase 7 step 45's minimal ITS grows to what real hardware needs: a two-level device table, one LPI property table shared by every redistributor and a pending table per redistributor, LPI IDs capped at 16 bits, all sized once at boot from `GITS_TYPER`/`GITS_BASER`; every PCI device takes MSI-X with one vector per queue routed to the queue's core. e2e: `test=shell` on `-device nvme` with `-M virt,gic-version=3,its=on` at `-smp 4`; each I/O queue's completions arrive on its own core (per-core counters). |
| 68 | Boot from firmware | The image starts with the Linux arm64 header (64 bytes, magic `ARM\x64`, image size, flags) and doubles as a PE/COFF EFI application (`MZ` in `code0` as Linux's EFI stub does, not re-verified; the PE offset in `res5`). Entered at EL2 or EL1 (`CurrentEL`). At EL2 it initializes EL2 completely before dropping to EL1: `HCR_EL2.RW`, `ICC_SRE_EL2` (system-register GIC access for EL1), `CPTR_EL2` and `MDCR_EL2` (no traps of FP, timers or debug), `VPIDR_EL2`/`VMPIDR_EL2` from the real IDs, `CNTVOFF_EL2` = 0 and timer access in `CNTHCTL_EL2`; it leaves a hyp stub (vectors that only let EL1 reinstall EL2's later, as Linux's `__hyp_stub_vectors`, general knowledge). The DTB comes from x0, not RAM base. The PSCI conduit (`hvc` or `smc`) comes from the DT's `psci` node, as 60a's SMCCC discovery already reads it: at EL2 an `hvc` would trap to the kernel itself, and `shutdown` and `CPU_ON` hard-code `hvc #0` today. The EFI entry takes the memory map and the DTB from the configuration table, exits boot services, cleans the image to the point of coherency, turns the MMU off, moves the image to a 2 MiB-aligned base and joins the same path. e2e: `-kernel Image` at EL1; at EL2 (`-M virt,virtualization=on`); and through edk2 (QEMU's `edk2-aarch64-code.fd`, `-M virt,acpi=off`) loading `\EFI\BOOT\BOOTAA64.EFI` from a FAT disk; each reaches `test=shell`. |
| 68a | ACPI static tables | A safe parser in a new pure crate (host-tested, seeded mutation) for RSDP, XSDT, MADT (GIC, CPUs), GTDT (timer), SPCR (console), MCFG (PCIe ECAM), IORT (ITS and SMMU mapping) and SRAT; no AML. The board takes its description from the DTB when firmware gives one, otherwise ACPI, behind one description type. e2e: edk2 with `-M virt,acpi=on` and no DTB reaches `test=shell` on NVMe. |
| 69 | SMMUv3 DMA isolation | Every DMA-capable PCI device gets its own stream with a stage-1 table holding only its buffers: rings and fixed pools mapped once at probe; page-cache frames mapped into the device's stream when the cache fills them and unmapped in batches at eviction, so a read or write maps nothing per request. A DMA buffer is a typestate: `Dma<Device>` becomes `Dma<Idle>` only after a `CMD_SYNC` covering its invalidation has completed, so the CPU cannot reuse a buffer the device can still reach, at no run-time cost. A device fault (an access outside its window) resets the device and fails its in-flight requests with `EIO`, never the kernel. virtio devices sit behind the SMMU only through the virtio-pci transport with `VIRTIO_F_ACCESS_PLATFORM` (`iommu_platform=on`); the step adds that transport. e2e on `-M virt,iommu=smmuv3`: NVMe runs the shell test and virtio-net-pci runs `test=httpd`; a test hook makes the NVMe device DMA outside its window, which is reported, the device reset, and the next read succeeds. |
| 70 | xHCI, HID, mass storage | An xHCI driver with USB HID keyboards (feeding phase 9's `tty` service beside the UART) and mass storage (bulk-only, SCSI) implementing the batched `Disk`; descriptor and SCSI parsers in a pure crate, host-tested with seeded mutation. e2e: `-device qemu-xhci -device usb-kbd -device usb-storage`: keys sent through QEMU's monitor reach msh, and MogFS mounts from the USB disk. |
| 71 | First real board: Radxa Orion O6 | A `crates/board/orion-o6` crate (memory map, UART, the board's Ethernet and NVMe, the SCMI performance driver), booted by its UEFI firmware (68, 68a). It reaches msh over serial, runs the shell test from NVMe and `fetch`/`httpd` over its NIC; 60a's report runs on real silicon, and firmware workarounds (SMCCC 1.1 through the board's TF-A) are called where its cores need them. Frequency: SCMI performance levels with one fixed rule (a domain with any busy core at its maximum, a fully idle domain at its minimum), measured for power and the tracked benchmarks. `scripts/board.sh` drives the run over the serial port. Recorded: boot time to the `boot:` line, the per-call benchmarks, the cross-OS rows against Linux on the same board, and the debt ledger's rows priced on hardware (v2 and SSB firmware calls, PAuth's key switch). |
| 71a | IPv6 and NDP | Moved here from phase 8 (research): IPv6, ICMPv6 and NDP in `crates/net` (pure, host-tested over the simulated link with the same loss and mutation tests as step 46), dual-stack sockets. e2e: ping6 to QEMU's user-network gateway and a TCP echo over IPv6; on the board, SLAAC on the LAN. |
| 72 | Tickless idle and idle states | No periodic tick: each core programs its timer for its next deadline (slice end, sleeper, network timer) and an idle core with none programs nothing. Idle states come from the DT's `idle-states` (or ACPI LPI with 68a) and are entered with PSCI `CPU_SUSPEND`. Rule: `wfi` first; a deeper state only once the core has idled for that state's target residency, and never past the next deadline. e2e: an idle `-smp 4` boot takes no timer interrupt on cores 1-3 over one second (counters); with a test DT that declares `idle-states` (QEMU's DT has none, and its `CPU_SUSPEND` is a `wfi`), the state sequence and wake times match the rule. On the board: idle power recorded with a USB-C meter. |
| 73 | Suspend to idle | Every driver implements `suspend` and `resume` (required trait methods, so none can forget); suspend-to-idle quiesces every device, parks every core in its deepest idle state and wakes on the UART or the PL031 RTC alarm. PSCI `SYSTEM_SUSPEND` (QEMU does not offer it) waits for a board that does. e2e: `test=suspend` suspends, an RTC alarm wakes it, and a disk read and `fetch` work afterwards. On the board: suspend power and resume time recorded. |

### Step details

- **67.** Benchmark: NVMe IOPS and latency at queue depth 1 and 32 against phase 7 step 45's numbers;
  interrupt-to-completion latency recorded. Invariants: an LPI is routed only to a core that owns the queue; ITS tables
  are sized once at boot: LPI IDs capped at 16 bits (the property table's size follows), the device table two-level,
  each bounded by what `GITS_TYPER`/`GITS_BASER` report. Avoids: interrupt sharing and a global vector allocator.
- **68.** Benchmark: boot (direct `-kernel`) holds; firmware boot time recorded. Invariants: one entry path after
  the first instructions, whatever the firmware; EL2 keeps only the stub, so phase 12's hypervisor can take it back;
  the image is signable for UEFI Secure Boot as is. Avoids: a separate bootloader (Linux's EFI stub shape: the kernel
  is its own EFI application).
- **68a.** Invariants: a table's length and checksum are checked before any field is read; unknown table revisions
  are rejected, not guessed. Avoids: AML (power buttons and hotplug wait until a target needs them).
- **69.** Benchmark: NVMe and virtio-net-pci throughput against `iommu=none`, which must hold: the per-request cost
  is designed out (mappings follow the cache's fill and eviction), and a slowdown is a failure like any other. The
  tracked QEMU benchmark runs stay at `iommu=none`. Invariants: no device can reach memory outside its window; a
  buffer leaves `Dma<Device>` only after its invalidation's `CMD_SYNC`. Avoids: per-packet and per-request map and
  unmap.
- **70.** Invariants: every descriptor field is range-checked once when decoded; a malformed device is ignored and
  counted, never a panic. Avoids: USB quirk tables as code paths (a quirk is data).
- **71.** Board drivers follow the existing rules: drivers in the kernel (ROADMAP Decided), `unsafe` only in the
  board crate behind safe wrappers. Phase 10 step 66b (MTE) follows this step.
- **71a.** Invariants: step 46's (fixed memory, every field range-checked once, a crafted packet dropped and
  counted); neighbour entries are learned only from solicited advertisements.
- **72.** Benchmark: yield, pipe and the wake path (`bench-ipi`) hold; idle-exit latency per state recorded.
  Invariants: a core never sleeps past its earliest deadline; RT-class wakes are never delayed by a state deeper than
  their latency allows. Avoids: cpuidle's menu governor heuristics.
- **73.** Invariants: a device reachable after resume is in the state it was in before suspend, or its users get
  `EIO`; nothing is lost silently. Avoids: drivers without suspend support (the trait requires it).

## Decided

- **First board: Radxa Orion O6 (CIX P1 CD8180), 16 or 32 GB** (research, 2026-10-07): Armv9 cores (Cortex-A720 and
  A520) with UEFI (edk2), NVMe, PCIe 4, and MTE available through an edk2 option (Radxa forum,
  forum.radxa.com/t/mte-feature-is-now-available-to-developers/28156), so 66a and 66b are priced on silicon. Not the
  64 GB model: a tag-storage base bug leaves about 28 GB usable. Sources: the CIX P1 device-tree patch on LKML
  (lkml.rescloud.iu.edu/2502.3/10603.html), Radxa's UART documentation, CNX Software's preview. Fallback: Radxa Rock
  5B (RK3588, edk2-rk3588, Linux's GIC erratum 3588001). Rejected: Raspberry Pi 5 (GIC-400, GICv2, which phase 5
  deleted), Ampere Altra (no MTE, cost), Apple silicon (AIC instead of a GIC, no PSCI).
- **Boot image: the Linux arm64 `Image` protocol and an EFI stub in one file** (coordinator, 2026-10-07): every
  arm64 firmware path speaks one of the two: QEMU `-kernel`, U-Boot `booti`, the Raspberry Pi firmware and UEFI
  (docs.kernel.org/arch/arm64/booting.html: the 64-byte header, `res5` the PE/COFF offset, x0 the DTB's physical
  address, x1-x3 zero). Which firmware a board ships then does not matter, and no bootloader of our own is written.
- **DT when firmware gives one, ACPI otherwise** (68a): the Orion O6 makes ACPI likely, so 68a is scheduled, not
  conditional.
- **Drivers stay in the kernel** (ROADMAP Decided): survey step 69's question is settled, so 69 is DMA isolation
  only.
- **Idle and frequency choices are fixed rules** (72, 71): `wfi` first and a deeper state only after its target
  residency, no prediction or tunables; frequency is set on the board through SCMI by one rule, since the scheduler
  tracks no utilization signal and QEMU offers no frequency control to test against.
- **Dropped, not scheduled** (research, 2026-10-07): NUMA (no target has it) and kexec-style fast reboot (no
  consumer).

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12): 67's core (the ITS for MSI) moved to
  phase 7 step 45, since QEMU `virt` has no GICv2m with GICv3; 67 keeps scaling it. 68 is split into 68 and 68a; 69
  drops the driver decision; 70-71 keep theirs; 71a is IPv6 from phase 8; 72 is split into 72 and 73 (suspend added,
  DVFS moved to 71); survey 73 (NUMA, kexec) is dropped.
- QEMU 9.2.1 implements PSCI `CPU_SUSPEND` as a `wfi`, does not offer `SYSTEM_SUSPEND`, and its `virt` DT declares no
  idle states (coordinator's review); 72 and 73's e2e test only what QEMU supports, and the board records the rest.

## What was done

Filled in as each step lands.
