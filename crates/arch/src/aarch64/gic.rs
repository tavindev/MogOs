use mm::PhysAddr;

const GICD_CTLR: u64 = 0x000;
const GICD_ISENABLER: u64 = 0x100;
const GICC_CTLR: u64 = 0x000;
const GICC_PMR: u64 = 0x004;
const GICC_IAR: u64 = 0x00c;
const GICC_EOIR: u64 = 0x010;

/// Turns on the GICv2 distributor at `dist` and CPU interface at `cpu`, and enables interrupt `irq`.
///
/// # Safety
///
/// `dist` and `cpu` must be a GICv2's distributor and CPU interface, mapped as Device memory.
pub unsafe fn enable(dist: PhysAddr, cpu: PhysAddr, irq: u32) {
    let isenabler = dist.0 + GICD_ISENABLER + 4 * (irq / 32) as u64;
    let writes = [
        (isenabler, 1 << (irq % 32)),
        (dist.0 + GICD_CTLR, 1),
        (cpu.0 + GICC_PMR, 0xff),
        (cpu.0 + GICC_CTLR, 1),
    ];
    for (addr, value) in writes {
        // SAFETY: the caller guarantees these are this GIC's registers.
        unsafe { (addr as *mut u32).write_volatile(value) };
    }
}

/// Acknowledges the highest-priority pending interrupt and returns its `GICC_IAR` (1023 if spurious).
///
/// # Safety
///
/// `cpu` must be a GICv2 CPU interface, mapped as Device memory.
pub unsafe fn ack(cpu: PhysAddr) -> u32 {
    // SAFETY: the caller guarantees `cpu` is a GICv2 CPU interface.
    unsafe { ((cpu.0 + GICC_IAR) as *const u32).read_volatile() }
}

/// Signals end of interrupt for `iar`, a value returned by `ack`.
///
/// # Safety
///
/// `cpu` must be a GICv2 CPU interface, mapped as Device memory.
pub unsafe fn eoi(cpu: PhysAddr, iar: u32) {
    // SAFETY: the caller guarantees `cpu` is a GICv2 CPU interface.
    unsafe { ((cpu.0 + GICC_EOIR) as *mut u32).write_volatile(iar) }
}
