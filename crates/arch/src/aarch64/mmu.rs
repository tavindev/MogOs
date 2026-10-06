use core::arch::asm;

use mm::PhysAddr;

/// Memory type of a mapping; the value is its `MAIR_EL1` attribute index.
pub enum MemoryType {
    Device = 0,
    Normal = 1,
}

/// `MAIR_EL1` matching `MemoryType`: Device-nGnRE, Normal write-back cacheable.
pub const MAIR: u64 = 0x04 | 0xff << 8;

const VALID_BLOCK: u64 = 0b01;
const VALID_TABLE_OR_PAGE: u64 = 0b11;
const INNER_SHAREABLE: u64 = 0b11 << 8;
const ACCESS_FLAG: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const AP_EL0: u64 = 1 << 6;
const AP_READ_ONLY: u64 = 1 << 7;
/// Output address bits of a 4 KiB page or table descriptor.
const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Level-1 block descriptor (4 KiB granule) mapping the 1 GiB at `addr` for EL1 read/write only, global.
pub const fn l1_block(addr: PhysAddr, ty: MemoryType) -> u64 {
    let attrs = match ty {
        MemoryType::Device => PXN | UXN,
        MemoryType::Normal => INNER_SHAREABLE | UXN,
    };
    addr.0 & 0x0000_ffff_c000_0000 | attrs | ACCESS_FLAG | (ty as u64) << 2 | VALID_BLOCK
}

/// Level-1 or level-2 descriptor pointing at the next-level table in the frame at `addr`.
pub const fn table_entry(addr: PhysAddr) -> u64 {
    addr.0 & ADDR | VALID_TABLE_OR_PAGE
}

/// What EL0 may do with a user page; EL1 never executes one.
pub enum UserAccess {
    ReadExecute,
    ReadWrite,
}

/// Level-3 descriptor mapping the 4 KiB of normal memory at `addr` for EL0, tagged with the ASID (not global).
pub const fn user_page(addr: PhysAddr, access: UserAccess) -> u64 {
    let access = match access {
        UserAccess::ReadExecute => AP_EL0 | AP_READ_ONLY,
        UserAccess::ReadWrite => AP_EL0 | UXN,
    };
    addr.0 & ADDR
        | PXN
        | access
        | NOT_GLOBAL
        | ACCESS_FLAG
        | INNER_SHAREABLE
        | (MemoryType::Normal as u64) << 2
        | VALID_TABLE_OR_PAGE
}

// Checked at build time: TCG ignores memory attributes and permissions, so a wrong bit would still boot.
const _: () = assert!(l1_block(PhysAddr(0), MemoryType::Device) == 0x0060_0000_0000_0401);
const _: () = assert!(l1_block(PhysAddr(0x4000_0000), MemoryType::Normal) == 0x0040_0000_4000_0705);
const _: () = assert!(table_entry(PhysAddr(0x4000_3000)) == 0x0000_0000_4000_3003);
const _: () =
    assert!(user_page(PhysAddr(0x4000_1000), UserAccess::ReadExecute) == 0x0020_0000_4000_1fc7);
const _: () =
    assert!(user_page(PhysAddr(0x4000_2000), UserAccess::ReadWrite) == 0x0060_0000_4000_2f47);

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

/// The boot level-1 table that `enable_mmu` loaded, with ASID 0.
pub fn boot_table() -> PhysAddr {
    PhysAddr(&raw const L1 as u64)
}

/// Maps the 4 KiB page at `va` to `leaf` (a `user_page` descriptor) in the tables under `l1`, taking each
/// missing level-2 or level-3 table from `alloc`; `None` if `alloc` ran out.
///
/// # Safety
///
/// `l1`, every table it points to and every frame `alloc` returns must be identity-mapped, zeroed (when
/// new) frames that only this address space uses; `va` must be at or above 4 GiB, below 512 GiB.
pub unsafe fn map_page(
    l1: PhysAddr,
    va: u64,
    leaf: u64,
    mut alloc: impl FnMut() -> Option<PhysAddr>,
) -> Option<()> {
    let mut table = l1.0;
    for shift in [30, 21] {
        let entry = (table as *mut u64).wrapping_add((va >> shift) as usize & 511);
        // SAFETY: the caller guarantees `table` is an identity-mapped table frame of this address space.
        let mut desc = unsafe { entry.read() };
        if desc & VALID_TABLE_OR_PAGE == 0 {
            desc = table_entry(alloc()?);
            // SAFETY: as above.
            unsafe { entry.write(desc) };
        }
        table = desc & ADDR;
    }
    // SAFETY: as above; the walk only follows table descriptors this address space owns.
    unsafe {
        (table as *mut u64)
            .wrapping_add((va >> 12) as usize & 511)
            .write(leaf)
    };
    // SAFETY: a barrier only orders the table writes before later walks.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    Some(())
}

/// Switches TTBR0 to the level-1 table `table`, its walks and TLB entries tagged with `asid`.
///
/// # Safety
///
/// `table` must be a level-1 table holding the kernel blocks, so the running code and data stay mapped, and
/// `asid` must belong to it alone (no stale TLB entries from another table).
pub unsafe fn set_ttbr0(table: PhysAddr, asid: usize) {
    // SAFETY: the caller guarantees the table keeps the kernel mapped and the ASID is its own.
    unsafe {
        asm!(
            "msr ttbr0_el1, {}",
            "isb",
            in(reg) table.0 | (asid as u64) << 48,
            options(nostack, preserves_flags)
        )
    };
}

/// Drops every non-global TLB entry tagged with `asid`; call after TTBR0 stopped using it.
pub fn flush_asid(asid: usize) {
    // SAFETY: invalidating TLB entries only forces later walks.
    unsafe {
        asm!(
            "tlbi aside1, {}",
            "dsb ish",
            "isb",
            in(reg) (asid as u64) << 48,
            options(nostack, preserves_flags)
        )
    };
}
