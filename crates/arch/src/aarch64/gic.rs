use mm::PhysAddr;

const GICD_CTLR: u64 = 0x000;
const GICD_ISENABLER: u64 = 0x100;
const GICD_ITARGETSR: u64 = 0x800;
const GICD_ICFGR: u64 = 0xc00;
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

/// Routes SPI `irq` of the GICv2 distributor at `dist` to CPU 0, edge-triggered if `edge`, else level-sensitive.
///
/// # Safety
///
/// `dist` must be a GICv2 distributor, mapped as Device memory, and `irq` an SPI (32 or above) it implements.
pub unsafe fn route_spi(dist: PhysAddr, irq: u32, edge: bool) {
    let target = (dist.0 + GICD_ITARGETSR + irq as u64) as *mut u8;
    let icfgr = (dist.0 + GICD_ICFGR + 4 * (irq / 16) as u64) as *mut u32;
    let bit = 2 << (2 * (irq % 16));
    // SAFETY: the caller guarantees `dist` is a GICv2 distributor implementing `irq`; ITARGETSR is byte-accessible.
    unsafe { target.write_volatile(1) };
    // SAFETY: as above.
    let config = unsafe { icfgr.read_volatile() } & !bit;
    // SAFETY: as above.
    unsafe { icfgr.write_volatile(if edge { config | bit } else { config }) };
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
