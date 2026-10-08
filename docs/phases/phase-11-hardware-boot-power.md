# Phase 11: Real hardware, boot, power

Goal (milestone): MogOs boots from firmware on one real arm64 board to msh with console, NVMe storage and network,
runs the cross-OS benchmarks there against Linux on the same board, and idles and suspends without a periodic tick.
QEMU stays the gate: every step has a QEMU e2e (`gic-version=3,its=on`, edk2 as firmware, `iommu=smmuv3`,
`qemu-xhci`); board runs are scripted over its serial console and recorded in What was done.

## Steps

Done-whens name the e2e test, the benchmarks held and added, and the invariants (Step details). Benchmarks are hvf
medians of 21 interleaved boots against the step's base commit (`docs/BENCHMARKS.md`): yield, syscall, pipe and boot
at `-smp 1` and `-smp 4`, plus the block and network rows a step names; "hold" means within noise, and a slowdown is a
failure, removed or shown unavoidable with numbers.

Needs: phase 6's higher-half, relocatable kernel (68 loads at whatever address firmware picks), phase 7 step 45 (PCIe
and NVMe on GICv2m), phase 5 step 29 (the fair class's slice timer, for 72). 67 and 68 run in parallel; 68a only if
the board chosen for 71 is ACPI-only (open question); 69 and 70 follow 67; 71 needs 67, 68 and the board; 72 can run
any time after phase 5; 73 follows 72.

| # | Step | Done when |
| --- | --- | --- |
| 67 | GIC ITS and MSI-X | `arch`'s GICv3 driver gains the ITS: command queue, device and collection tables sized from `GITS_TYPER`/`GITS_BASER` at boot, LPI property and pending tables per redistributor; NVMe and every PCI device take MSI-X through it, one vector per queue routed to the queue's core; GICv2m goes (hard cutover: GICv3 is the only GIC, phase 5 Decided). e2e: `test=shell` on `-device nvme` with `-M virt,gic-version=3,its=on` at `-smp 4`; each I/O queue's completions arrive on its own core (per-core counters). |
| 68 | Boot from firmware | The image starts with the Linux arm64 header (64 bytes, magic `ARM\x64`, image size, flags) and doubles as a PE/COFF EFI application (`MZ` in `code0` as Linux's EFI stub does, not re-verified; the PE offset in `res5`). Entered at EL2 or EL1 (`CurrentEL`); at EL2 it installs a hyp stub (vectors that only let EL1 reinstall EL2's later, as Linux's `__hyp_stub_vectors`, general knowledge), sets `HCR_EL2.RW` and timer access, and drops to EL1. The DTB comes from x0, not RAM base. The EFI entry takes the memory map and the DTB (or ACPI tables) from the configuration table, exits boot services and joins the same path. e2e: `-kernel Image` at EL1; at EL2 (`-M virt,virtualization=on`); and through edk2 (QEMU's `edk2-aarch64-code.fd`) loading `\EFI\BOOT\BOOTAA64.EFI` from a FAT disk; each reaches `test=shell`. |
| 68a | ACPI static tables (if the board needs them) | A safe parser in a new pure crate (host-tested, seeded mutation) for RSDP, XSDT, MADT (GIC, CPUs), GTDT (timer), SPCR (console), MCFG (PCIe ECAM) and SRAT; no AML. The board takes its description from the DTB when firmware gives one, otherwise ACPI, behind one description type. e2e: edk2 with `-M virt,acpi=on` and no DTB reaches `test=shell` on NVMe. |
| 69 | SMMUv3 DMA isolation | Every DMA-capable device gets its own stream with a stage-1 table holding only its buffers: rings and fixed pools mapped once at probe, data buffers per request with invalidations batched per completion batch. A DMA buffer is a typestate (`Dma<Idle>` / `Dma<Device>`): the CPU cannot touch a buffer the device owns, at no run-time cost. A device fault (an access outside its window) resets the device and fails its in-flight requests with `EIO`, never the kernel. e2e on `-M virt,iommu=smmuv3`: NVMe and virtio-net run the shell and `test=httpd`; a test hook makes the NVMe device DMA outside its window, which is reported, the device reset, and the next read succeeds. |
| 70 | xHCI, HID, mass storage | An xHCI driver with USB HID keyboards (feeding phase 9's `tty` service beside the UART) and mass storage (bulk-only, SCSI) implementing the batched `Disk`; descriptor and SCSI parsers in a pure crate, host-tested with seeded mutation. e2e: `-device qemu-xhci -device usb-kbd -device usb-storage`: keys sent through QEMU's monitor reach msh, and MogFS mounts from the USB disk. |
| 71 | First real board | A `crates/board/<board>` crate (memory map, UART, the board's Ethernet and storage), booted by its UEFI firmware (68). It reaches msh over serial, runs the shell test from NVMe and `fetch`/`httpd` over its NIC; 60a's report runs on real silicon, and firmware workarounds (SMCCC 1.1 through the board's TF-A) are called where its cores need them. `scripts/board.sh` drives the run over the serial port. Recorded: boot time to the `boot:` line, the per-call benchmarks, the cross-OS rows against Linux on the same board, and the debt ledger's rows priced on hardware (v2 and SSB firmware calls, PAuth's key switch if present). |
| 72 | Tickless idle and idle states | No periodic tick: each core programs its timer for its next deadline (slice end, sleeper, network timer) and an idle core with none programs nothing. Idle states come from the DT's `idle-states` (or ACPI LPI with 68a) and are entered with PSCI `CPU_SUSPEND`: the deepest state whose target residency fits before the core's next deadline, no prediction. e2e: an idle `-smp 4` boot takes no timer interrupt on cores 1-3 over one second (counters); a sleeper's wake lands within its deadline plus the state's exit latency. On the board: idle power recorded with a USB-C meter. |
| 73 | System suspend and DVFS | Every driver implements `suspend` and `resume` (required trait methods, so none can forget); suspend-to-idle quiesces every device, parks every core in its deepest state and wakes on the UART or the PL031 RTC alarm; PSCI `SYSTEM_SUSPEND` is used where `PSCI_FEATURES` reports it. DVFS: each frequency domain's performance level follows the utilization the fair class already tracks (schedutil's rule, frequency = 1.25 x max x util / capacity), set through SCMI or the board's clock driver; RT-class tasks run at the domain's maximum. e2e: `test=suspend` suspends, an RTC alarm wakes it, and a disk read and `fetch` work afterwards; the governor's mapping is host-tested. On the board: suspend power, resume time and the benchmarks under DVFS recorded. |

### Step details

- **67.** Benchmark: NVMe IOPS and latency at queue depth 1 and 32 against GICv2m (phase 7 step 45's numbers);
  interrupt-to-completion latency recorded. Invariants: an LPI is routed only to a core that owns the queue; ITS tables
  are sized once at boot from the hardware's own limits. Avoids: interrupt sharing and a global vector allocator.
- **68.** Benchmark: boot (direct `-kernel`) holds; firmware boot time recorded. Invariants: one entry path after
  the first instructions, whatever the firmware; EL2 keeps only the stub, so phase 12's hypervisor can take it back;
  the image is signable for UEFI Secure Boot as is. Avoids: a separate bootloader (Linux's EFI stub shape: the kernel
  is its own EFI application).
- **68a.** Invariants: a table's length and checksum are checked before any field is read; unknown table revisions
  are rejected, not guessed. Avoids: AML (power buttons and hotplug wait until a target needs them).
- **69.** Benchmark: NVMe and virtio-net throughput against `iommu=none`; a cost is recorded and paid (isolation is
  a safety requirement on hardware), and the fixed-ring design keeps it to the per-request data mappings.
  Invariants: no device can reach memory outside its window; a buffer is never unmapped while the device owns it
  (typestate). Avoids: per-packet map and unmap on the network path (its pool is mapped once).
- **70.** Invariants: every descriptor field is range-checked once when decoded; a malformed device is ignored and
  counted, never a panic. Avoids: USB quirk tables as code paths (a quirk is data).
- **71.** Board drivers follow the existing rules: drivers in the kernel (ROADMAP Decided), `unsafe` only in the
  board crate behind safe wrappers.
- **72.** Benchmark: yield, pipe and the wake path (`bench-ipi`) hold; idle-exit latency per state recorded.
  Invariants: a core never sleeps past its earliest deadline; RT-class wakes are never delayed by a state deeper than
  their latency allows. Avoids: cpuidle's menu governor heuristics.
- **73.** Invariants: a device reachable after resume is in the state it was in before suspend, or its users get
  `EIO`; nothing is lost silently. Avoids: drivers without suspend support (the trait requires it) and governor
  zoos (one rule).

## Decided

- **Boot image: the Linux arm64 `Image` protocol and an EFI stub in one file** (coordinator, 2026-10-07): every
  arm64 firmware path speaks one of the two: QEMU `-kernel`, U-Boot `booti`, the Raspberry Pi firmware and UEFI
  (docs.kernel.org/arch/arm64/booting.html: the 64-byte header, `res5` the PE/COFF offset, x0 the DTB's physical
  address, x1-x3 zero). Which firmware a board ships then does not matter, and no bootloader of our own is written.
- **DT first, ACPI only when a target needs it**: QEMU, U-Boot and the RK3588 and Raspberry Pi firmware hand over a
  DTB; ACPI-only machines (arm servers under SBBR, AWS Graviton) wait for 68a.
- **Drivers stay in the kernel** (ROADMAP Decided): survey step 69's question is settled, so 69 is DMA isolation
  only.
- **Idle and frequency choices are deterministic rules** (72, 73): the deepest state that fits before the next
  deadline, schedutil's utilization rule (docs.kernel.org/admin-guide/pm/cpufreq.html, from the survey's link; the 1.25 factor not re-read), no prediction or tunables.

## Notes

- Board candidates (for the user, see the open question in the report), from the survey and this session's search:
  - Raspberry Pi 5: GIC-400, GICv2 (RTEMS BSP docs); phase 5 deleted GICv2, so it would come back from history; its
    firmware loads `kernel_2712.img`/`kernel8.img` (config.txt docs); x0 = DTB is inferred from the arm64 protocol,
    not verified.
  - RK3588 boards (Radxa Rock 5B, Orange Pi 5): UEFI from edk2-rk3588 with PCIe and NVMe working (its GitHub);
    GIC-600 with ITS is believed, not verified, and Linux carries a Rockchip GIC erratum (3588001, not read).
    Cortex-A76/A55: no PAuth, BTI or MTE.
  - Radxa Orion O6 (CIX P1, Cortex-A720 and A520): UEFI (edk2), NVMe, PCIe 4 (CNX Software); GICv3 and whether PAC,
    BTI and MTE are exposed are not verified. If they are, it is the only candidate that prices 66a and 66b on
    silicon.
  - AmpereOne servers have MTE (per the MTE papers' abstracts); ACPI-only, so 68a first.
- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12): 67 keeps GICv3 and MSI (GICv3 itself
  landed in phase 5 step 25c, so only the ITS is left); 68 is split into 68 and 68a; 69 drops the driver decision;
  70-71 keep theirs; 72 is split into 72 and 73 with system suspend added; survey 73 (NUMA, kexec fast reboot) is not
  scheduled: no target has NUMA, and fast reboot has no consumer (phase 12's live update was dropped too).
- QEMU 9.2.1's handling of PSCI `CPU_SUSPEND` (power-down states versus standby) and `SYSTEM_SUSPEND` is not
  verified; 72 and 73's e2e assert what `PSCI_FEATURES` reports and test the paths QEMU supports.

## What was done

Filled in as each step lands.
