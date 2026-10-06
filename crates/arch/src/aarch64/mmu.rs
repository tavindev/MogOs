use core::arch::asm;

use mm::PhysAddr;

/// Memory type of a mapping; the value is its `MAIR_EL1` attribute index.
pub enum MemoryType {
    Device = 0,
    Normal = 1,
}

/// `MAIR_EL1` matching `MemoryType`: Device-nGnRE, Normal write-back cacheable.
pub const MAIR: u64 = 0x04 | 0xff << 8;

/// Level-1 block descriptor (4 KiB granule) mapping the 1 GiB at `addr` for EL1 read/write.
pub const fn l1_block(addr: PhysAddr, ty: MemoryType) -> u64 {
    const VALID_BLOCK: u64 = 0b01;
    const INNER_SHAREABLE: u64 = 0b11 << 8;
    const ACCESS_FLAG: u64 = 1 << 10;
    const PXN_UXN: u64 = 0b11 << 53;
    let attrs = match ty {
        MemoryType::Device => PXN_UXN,
        MemoryType::Normal => INNER_SHAREABLE,
    };
    addr.0 & 0x0000_ffff_c000_0000 | attrs | ACCESS_FLAG | (ty as u64) << 2 | VALID_BLOCK
}

// Checked at build time: TCG ignores memory attributes, so a wrong bit would still boot.
const _: () = assert!(l1_block(PhysAddr(0), MemoryType::Device) == 0x0060_0000_0000_0401);
const _: () = assert!(l1_block(PhysAddr(0x4000_0000), MemoryType::Normal) == 0x0000_0000_4000_0705);

#[repr(C, align(4096))]
struct Table([u64; 512]);

static mut L1: Table = Table([0; 512]);

/// Loads `l1` as the level-1 table for TTBR0 (4 KiB granule, 39-bit VA, attributes per `mair`) and turns on the MMU and caches.
///
/// # Safety
///
/// Call with the MMU off (the table is overwritten in place, no break-before-make) and before any
/// atomic read-modify-write (exclusives need Normal memory). The entries must map, at their current physical addresses, all code, data,
/// stack and MMIO the program uses.
pub unsafe fn enable_mmu(l1: &[u64], mair: u64) {
    let table = &raw mut L1;
    // SAFETY: single core with the MMU off; nothing else references L1.
    unsafe { (&mut (*table).0)[..l1.len()].copy_from_slice(l1) };
    let pa_range: u64;
    // SAFETY: reading ID_AA64MMFR0_EL1 has no side effects.
    unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) pa_range) };
    let tcr = 25 // T0SZ: 39-bit VA
        | 0b01 << 8 | 0b01 << 10 | 0b11 << 12 // walks: write-back, inner shareable
        | 1 << 23 // EPD1: no TTBR1 walks
        | 0b10 << 30 // TG1 4 KiB, only to avoid the reserved encoding
        | (pa_range & 0x7) << 32;
    // SAFETY: the table is written and the caller guarantees it maps everything in use.
    unsafe {
        asm!(
            "dsb ish",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "mrs {t}, sctlr_el1",
            "orr {t}, {t}, {m_c_i}",
            "msr sctlr_el1, {t}",
            "isb",
            mair = in(reg) mair,
            tcr = in(reg) tcr,
            ttbr = in(reg) &raw const L1,
            m_c_i = in(reg) 1u64 << 0 | 1 << 2 | 1 << 12,
            t = out(reg) _,
        )
    }
}
