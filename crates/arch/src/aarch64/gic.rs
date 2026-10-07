use core::arch::asm;
use core::hint::spin_loop;

use mm::PhysAddr;

const GICD_CTLR: u64 = 0x0000;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_IROUTER: u64 = 0x6000;
/// Affinity routing, then Group 1 delivery (one security state: QEMU `virt` runs the GIC with `DS` set).
const CTLR_ARE: u32 = 1 << 4;
const CTLR_ENABLE_GRP1: u32 = 1 << 1;
/// `GICD_CTLR`'s register write pending.
const RWP: u32 = 1 << 31;
const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// The SGI and PPI frame follows each redistributor's control frame.
const GICR_SGI: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = GICR_SGI + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI + 0x0100;

/// # Safety
///
/// `addr` must be a GIC register, mapped as Device memory.
unsafe fn read(addr: u64) -> u32 {
    // SAFETY: the caller's contract.
    unsafe { (addr as *const u32).read_volatile() }
}

/// # Safety
///
/// As `read`.
unsafe fn write(addr: u64, value: u32) {
    // SAFETY: the caller's contract.
    unsafe { (addr as *mut u32).write_volatile(value) }
}

/// # Safety
///
/// `ctlr` must be a `GICD_CTLR`, mapped as Device memory.
unsafe fn wait_rwp(ctlr: u64) {
    // SAFETY: the caller's contract.
    while unsafe { read(ctlr) } & RWP != 0 {
        spin_loop();
    }
}

/// Turns on the GICv3 distributor at `dist`: affinity routing and Group 1 in one write, as Linux does (each register
/// access is a VM exit under hvf).
///
/// # Safety
///
/// `dist` must be a GICv3 distributor, mapped as Device memory; call once, before any core enables its interface.
pub unsafe fn enable(dist: PhysAddr) {
    // SAFETY: the caller guarantees `dist` is a GICv3 distributor.
    unsafe { write(dist.0 + GICD_CTLR, CTLR_ARE | CTLR_ENABLE_GRP1) };
    // SAFETY: as above.
    unsafe { wait_rwp(dist.0 + GICD_CTLR) };
}

/// The affinity (Aff3.Aff2.Aff1.Aff0, a byte each) of the redistributor at `redist`, as `GICR_TYPER` reports it.
///
/// # Safety
///
/// `redist` must be a GICv3 redistributor's control frame, mapped as Device memory.
pub unsafe fn affinity(redist: PhysAddr) -> u32 {
    // SAFETY: the caller's contract; the upper half of the 64-bit `GICR_TYPER` holds the affinity.
    unsafe { read(redist.0 + GICR_TYPER + 4) }
}

/// Wakes this core's redistributor at `redist`, puts its SGIs and PPIs in Group 1, and turns on its CPU interface (the
/// system registers), letting every priority through.
///
/// # Safety
///
/// `redist` must be this core's GICv3 redistributor (`affinity` matches its MPIDR), mapped as Device memory, after
/// `enable`.
pub unsafe fn enable_cpu(redist: PhysAddr) {
    // SAFETY: the caller guarantees `redist` is this core's redistributor; `GICR_WAKER`'s other writable bit is
    // implementation defined and 0 at reset.
    unsafe { write(redist.0 + GICR_WAKER, 0) };
    // SAFETY: as above.
    while unsafe { read(redist.0 + GICR_WAKER) } & WAKER_CHILDREN_ASLEEP != 0 {
        spin_loop();
    }
    // SAFETY: as above.
    unsafe { write(redist.0 + GICR_IGROUPR0, u32::MAX) };
    // SAFETY: ICC_SRE_EL1.SRE, ICC_PMR_EL1 and ICC_IGRPEN1_EL1 only configure this core's CPU interface; each `isb`
    // completes the write before the next use.
    unsafe {
        asm!(
            "msr icc_sre_el1, {sre}",
            "isb",
            "msr icc_pmr_el1, {pmr}",
            "isb",
            "msr icc_igrpen1_el1, {en}",
            "isb",
            sre = in(reg) 1u64,
            pmr = in(reg) 0xffu64,
            en = in(reg) 1u64,
            options(nostack, preserves_flags),
        )
    };
}

/// Puts shared peripheral interrupt `irq`, with the other 31 SPIs of its `GICD_IGROUPR` word, in Group 1 (every
/// interrupt the kernel uses is), and delivers it to the core with affinity `mpidr` only.
///
/// # Safety
///
/// `dist` must be a GICv3 distributor with affinity routing on, mapped as Device memory, and `irq` an SPI (32 or more).
pub unsafe fn route(dist: PhysAddr, irq: u32, mpidr: u64) {
    let group = dist.0 + GICD_IGROUPR + 4 * (irq / 32) as u64;
    // SAFETY: the caller guarantees `dist` is a GICv3 distributor.
    unsafe { write(group, u32::MAX) };
    let affinity = mpidr & 0xff_00ff_ffff;
    // SAFETY: as above; IRM (bit 31) clear routes to that one core.
    unsafe { ((dist.0 + GICD_IROUTER + 8 * irq as u64) as *mut u64).write_volatile(affinity) };
}

/// Unmasks shared peripheral interrupt `irq` in the distributor at `dist`.
///
/// # Safety
///
/// `dist` must be a GICv3 distributor, mapped as Device memory, and `irq` an SPI (32 or more).
pub unsafe fn unmask(dist: PhysAddr, irq: u32) {
    // SAFETY: the caller guarantees `dist` is a GICv3 distributor.
    unsafe {
        write(
            dist.0 + GICD_ISENABLER + 4 * (irq / 32) as u64,
            1 << (irq % 32),
        )
    };
}

/// Unmasks the SGIs and PPIs whose bits `irqs` sets (bit `n` for interrupt `n`, below 32) in this core's
/// redistributor at `redist`.
///
/// # Safety
///
/// `redist` must be this core's GICv3 redistributor, mapped as Device memory.
pub unsafe fn unmask_local(redist: PhysAddr, irqs: u32) {
    // SAFETY: the caller guarantees `redist` is this core's redistributor.
    unsafe { write(redist.0 + GICR_ISENABLER0, irqs) };
}

/// Sends SGI `sgi` (below 16) to the core with affinity `mpidr`, once the stores before it are visible to that core.
pub fn send_sgi(mpidr: u64, sgi: u32) {
    let aff0 = mpidr & 0xff;
    let value = 1 << (aff0 % 16) // the target list: 16 cores per write, its range selector RS above
        | (mpidr >> 8 & 0xff) << 16
        | (sgi as u64 & 0xf) << 24
        | (mpidr >> 16 & 0xff) << 32
        | (aff0 / 16) << 44
        | (mpidr >> 32 & 0xff) << 48;
    // SAFETY: an SGI only interrupts its target; `dsb ishst` orders the stores it announces before it, `isb` completes it.
    unsafe {
        asm!(
            "dsb ishst",
            "msr icc_sgi1r_el1, {}",
            "isb",
            in(reg) value,
            options(nostack, preserves_flags),
        )
    };
}

/// Acknowledges the highest-priority pending Group 1 interrupt and returns its ID (1023 if spurious).
pub fn ack() -> u32 {
    let iar: u64;
    // SAFETY: reading ICC_IAR1_EL1 acknowledges the interrupt, which only this core's handler then serves.
    unsafe { asm!("mrs {}, icc_iar1_el1", out(reg) iar, options(nostack, preserves_flags)) };
    iar as u32
}

/// Signals end of interrupt for `irq`, the ID `ack` returned.
pub fn eoi(irq: u32) {
    // SAFETY: writing ICC_EOIR1_EL1 only drops this core's running priority for `irq`.
    unsafe { asm!("msr icc_eoir1_el1, {}", in(reg) irq as u64, options(nostack, preserves_flags)) };
}
