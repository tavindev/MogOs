use mm::PhysAddr;

const GICD_CTLR: u64 = 0x000;
const GICD_ISENABLER: u64 = 0x100;
const GICD_ITARGETSR: u64 = 0x800;
const GICD_SGIR: u64 = 0xf00;
const GICC_CTLR: u64 = 0x000;
const GICC_PMR: u64 = 0x004;
const GICC_IAR: u64 = 0x00c;
const GICC_EOIR: u64 = 0x010;

/// Turns on the GICv2 distributor at `dist` and this core's CPU interface at `cpu`.
///
/// # Safety
///
/// `dist` and `cpu` must be a GICv2's distributor and CPU interface, mapped as Device memory.
pub unsafe fn enable(dist: PhysAddr, cpu: PhysAddr) {
    // SAFETY: the caller guarantees `dist` is a GICv2 distributor.
    unsafe { ((dist.0 + GICD_CTLR) as *mut u32).write_volatile(1) };
    // SAFETY: the caller guarantees `cpu` is a GICv2 CPU interface.
    unsafe { enable_cpu(cpu) }
}

/// Turns on this core's (banked) GICv2 CPU interface at `cpu`, letting every priority through.
///
/// # Safety
///
/// `cpu` must be a GICv2 CPU interface, mapped as Device memory.
pub unsafe fn enable_cpu(cpu: PhysAddr) {
    for (addr, value) in [(cpu.0 + GICC_PMR, 0xff), (cpu.0 + GICC_CTLR, 1)] {
        // SAFETY: the caller guarantees these are this GIC's registers.
        unsafe { (addr as *mut u32).write_volatile(value) };
    }
}

/// Delivers shared peripheral interrupt `irq` to the CPU interface numbered `cpu` only.
///
/// # Safety
///
/// `dist` must be a GICv2 distributor, mapped as Device memory, `irq` an SPI (32 or more) and `cpu` below 8.
pub unsafe fn route(dist: PhysAddr, irq: u32, cpu: usize) {
    // SAFETY: the caller guarantees `dist` is a GICv2 distributor; GICD_ITARGETSR is byte-accessible.
    unsafe { ((dist.0 + GICD_ITARGETSR + irq as u64) as *mut u8).write_volatile(1 << cpu) };
}

/// Unmasks interrupt `irq` in the GICv2 distributor at `dist`; an SGI or PPI (below 32) for this core only.
///
/// # Safety
///
/// `dist` must be a GICv2 distributor, mapped as Device memory.
pub unsafe fn unmask(dist: PhysAddr, irq: u32) {
    let isenabler = dist.0 + GICD_ISENABLER + 4 * (irq / 32) as u64;
    // SAFETY: the caller guarantees `dist` is a GICv2 distributor.
    unsafe { (isenabler as *mut u32).write_volatile(1 << (irq % 32)) };
}

/// Sends SGI `sgi` (below 16) to the CPU interface numbered `cpu`, once the stores before it are visible to that core.
///
/// # Safety
///
/// `dist` must be a GICv2 distributor, mapped as Device memory, and `cpu` below 8.
pub unsafe fn send_sgi(dist: PhysAddr, cpu: usize, sgi: u32) {
    // SAFETY: a barrier only orders the stores before the SGI.
    unsafe { core::arch::asm!("dsb ishst", options(nostack, preserves_flags)) };
    // SAFETY: the caller guarantees `dist` is a GICv2 distributor.
    unsafe { ((dist.0 + GICD_SGIR) as *mut u32).write_volatile(1 << (16 + cpu) | sgi) };
}

/// Acknowledges the highest-priority pending interrupt and returns its `GICC_IAR`: the ID in bits 0-9 (1023 if
/// spurious), an SGI's sender above it.
///
/// # Safety
///
/// `cpu` must be a GICv2 CPU interface, mapped as Device memory.
pub unsafe fn ack(cpu: PhysAddr) -> u32 {
    // SAFETY: the caller guarantees `cpu` is a GICv2 CPU interface.
    unsafe { ((cpu.0 + GICC_IAR) as *const u32).read_volatile() }
}

/// Signals end of interrupt for `iar`, the whole value `ack` returned.
///
/// # Safety
///
/// `cpu` must be a GICv2 CPU interface, mapped as Device memory.
pub unsafe fn eoi(cpu: PhysAddr, iar: u32) {
    // SAFETY: the caller guarantees `cpu` is a GICv2 CPU interface.
    unsafe { ((cpu.0 + GICC_EOIR) as *mut u32).write_volatile(iar) }
}
