use core::arch::{asm, global_asm};
use core::ops::Range;

use mm::PhysAddr;

/// `MAIR_EL1`: attribute 0 Device-nGnRE, 1 Normal write-back cacheable.
const MAIR: u64 = 0x04 | 0xff << 8;

const VALID_BLOCK: u64 = 0b01;
const VALID_TABLE_OR_PAGE: u64 = 0b11;
/// Attribute index 1, inner shareable.
const NORMAL: u64 = 1 << 2 | 0b11 << 8;
const ACCESS_FLAG: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const AP_EL0: u64 = 1 << 6;
const AP_READ_ONLY: u64 = 1 << 7;
/// Output address bits of a 4 KiB page or table descriptor.
const ADDR: u64 = 0x0000_ffff_ffff_f000;
const GIB: u64 = 1 << 30;
const BLOCK_2M: u64 = 1 << 21;
const PAGE: u64 = 1 << 12;

/// How EL1 may use a kernel mapping; EL0 never reaches one, and none is both writable and executable.
#[derive(Clone, Copy)]
enum Kernel {
    Device,
    ReadWrite,
    ReadOnly,
    Text,
}

/// A global, EL1-only block (`VALID_BLOCK`) or page (`VALID_TABLE_OR_PAGE`) descriptor for the aligned `addr`.
const fn kernel(addr: u64, access: Kernel, valid: u64) -> u64 {
    let attrs = match access {
        Kernel::Device => PXN,
        Kernel::ReadWrite => NORMAL | PXN,
        Kernel::ReadOnly => NORMAL | PXN | AP_READ_ONLY,
        Kernel::Text => NORMAL | AP_READ_ONLY,
    };
    addr | attrs | UXN | ACCESS_FLAG | valid
}

/// Level-1 or level-2 descriptor pointing at the next-level table in the frame at `addr`.
const fn table_entry(addr: PhysAddr) -> u64 {
    addr.0 & ADDR | VALID_TABLE_OR_PAGE
}

/// What EL0 may do with a user page; EL1 never executes one.
#[derive(Clone, Copy)]
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
    addr.0 & ADDR | PXN | access | NOT_GLOBAL | ACCESS_FLAG | NORMAL | VALID_TABLE_OR_PAGE
}

// Checked at build time: TCG enforces AP/XN but ignores cacheability attributes, so a wrong bit there would still boot.
const _: () = assert!(kernel(0, Kernel::Device, VALID_BLOCK) == 0x0060_0000_0000_0401);
const _: () = assert!(kernel(0x4000_0000, Kernel::ReadOnly, VALID_BLOCK) == 0x0060_0000_4000_0785);
const _: () = assert!(kernel(0x4040_0000, Kernel::ReadWrite, VALID_BLOCK) == 0x0060_0000_4040_0705);
const _: () =
    assert!(kernel(0x4020_0000, Kernel::Text, VALID_TABLE_OR_PAGE) == 0x0040_0000_4020_0787);
const _: () = assert!(table_entry(PhysAddr(0x4000_3000)) == 0x0000_0000_4000_3003);
const _: () =
    assert!(user_page(PhysAddr(0x4000_1000), UserAccess::ReadExecute) == 0x0020_0000_4000_1fc7);
const _: () =
    assert!(user_page(PhysAddr(0x4000_2000), UserAccess::ReadWrite) == 0x0060_0000_4000_2f47);

#[repr(C, align(4096))]
struct Table([u64; 512]);

static mut L1: Table = Table([0; 512]);
/// The RAM GiB's 2 MiB blocks, and its first block's 4 KiB pages.
static mut L2: Table = Table([0; 512]);
static mut L3: Table = Table([0; 512]);

/// TCR_EL1 but for the PA size, which `aarch64_mmu_on` reads from ID_AA64MMFR0_EL1.
const TCR: u64 = 25 // T0SZ: 39-bit VA
    | 0b01 << 8 | 0b01 << 10 | 0b11 << 12 // walks: write-back, inner shareable
    | 1 << 23 // EPD1: no TTBR1 walks
    | 0b10 << 30; // TG1 4 KiB, only to avoid the reserved encoding
const SCTLR: u64 = 1 << 0 | 1 << 2 | 1 << 12 // M, C, I: MMU, data and instruction caches on
    | 1 << 3 | 1 << 4 // SA, SA0: SP alignment checks at EL1 and EL0
    | 1 << 16 | 1 << 18 // nTWI, nTWE: EL0 wfi/wfe not trapped; EL0 cannot mask IRQs (UMA = 0), so a tick ends them
    | 1 << 19 // WXN: a writable mapping never executes
    | 1 << 23 // SPAN: PAN untouched on exception entry (the kernel reads checked user pages directly)
    | 1 << 11 | 1 << 20 | 1 << 22 | 1 << 28 | 1 << 29; // RES1 on ARMv8.0
// UMA, DZE, UCT, UCI = 0: EL0 cannot mask interrupts, zero or query caches, or maintain them; E0E, EE = 0: little endian.
const _: () = assert!(MAIR < 1 << 16 && TCR < 1 << 32 && SCTLR < 1 << 32);

// Every core's MMU-on, run with the MMU off, so no load, store or atomic before SCTLR is set; clobbers x9 and x10.
global_asm!(
    r#"
.text
.global aarch64_mmu_on
aarch64_mmu_on:
    dsb ish
    mov x9, #{mair}
    msr mair_el1, x9
    movz x9, #({tcr} & 0xffff)
    movk x9, #({tcr} >> 16), lsl #16
    mrs x10, id_aa64mmfr0_el1
    bfi x9, x10, #32, #3
    msr tcr_el1, x9
    adrp x9, {l1}
    add x9, x9, :lo12:{l1}
    msr ttbr0_el1, x9
    isb
    tlbi vmalle1
    dsb ish
    isb
    movz x9, #({sctlr} & 0xffff)
    movk x9, #({sctlr} >> 16), lsl #16
    msr sctlr_el1, x9
    isb
    ret
"#,
    mair = const MAIR,
    tcr = const TCR,
    sctlr = const SCTLR,
    l1 = sym L1,
);

/// The kernel's identity map, which `enable_mmu` builds: every entry EL1-only and global.
pub struct KernelMap {
    /// The GiB of device memory: PXN.
    pub device: PhysAddr,
    /// The GiB of RAM: 2 MiB blocks, read-write and PXN, but for its first and the image's.
    pub ram: PhysAddr,
    /// In RAM's first 2 MiB, mapped by 4 KiB pages: read-only and PXN; the rest of that block read-write and PXN but
    /// for the unmapped `guards`.
    pub dtb: Range<PhysAddr>,
    pub guards: [PhysAddr; super::MAX_CPUS],
    /// The image's 2 MiB blocks: read-only and executable below `text_end`, read-only and PXN below `rodata_end`,
    /// read-write and PXN above.
    pub image: PhysAddr,
    pub text_end: PhysAddr,
    pub rodata_end: PhysAddr,
}

/// Builds the boot tables for `map` (4 KiB granule, 39-bit VA, attributes per `MAIR`) and turns on the MMU, caches
/// and WXN with them; other cores turn theirs on with the same tables from `aarch64_secondary`.
///
/// # Safety
///
/// Call once, on core 0, with the MMU off (the tables are written in place, no break-before-make) and before any
/// atomic read-modify-write (exclusives need Normal memory). `map` must map, at their current physical addresses,
/// all code, data, stack and MMIO the program uses, with its text below `text_end`; the GiBs, `image`, `text_end` and
/// `rodata_end` aligned, the image past RAM's first 2 MiB.
pub unsafe fn enable_mmu(map: &KernelMap) {
    let (l1, l2, l3) = (&raw mut L1, &raw mut L2, &raw mut L3);
    // SAFETY: core 0 alone with the MMU off; nothing else references the tables.
    let l1 = unsafe { &mut (*l1).0 };
    // SAFETY: as above.
    let l2 = unsafe { &mut (*l2).0 };
    // SAFETY: as above.
    let l3 = unsafe { &mut (*l3).0 };
    l1[(map.device.0 / GIB) as usize] = kernel(map.device.0, Kernel::Device, VALID_BLOCK);
    l1[(map.ram.0 / GIB) as usize] = table_entry(PhysAddr(l2.as_ptr() as u64));
    // Plain stores of precomputed attributes: with the MMU off every access here is uncached.
    let fill = |table: &mut [u64], (start, end): (u64, u64), base: u64, size: u64, attrs: u64| {
        for i in start..end {
            table[i as usize] = (base + i * size) | attrs;
        }
    };
    let block = |addr: PhysAddr| (addr.0 - map.ram.0) / BLOCK_2M;
    let (image, text, rodata) = (block(map.image), block(map.text_end), block(map.rodata_end));
    let blocks = |access| kernel(0, access, VALID_BLOCK);
    fill(l2, (0, 512), map.ram.0, BLOCK_2M, blocks(Kernel::ReadWrite));
    fill(l2, (image, text), map.ram.0, BLOCK_2M, blocks(Kernel::Text));
    fill(
        l2,
        (text, rodata),
        map.ram.0,
        BLOCK_2M,
        blocks(Kernel::ReadOnly),
    );
    l2[0] = table_entry(PhysAddr(l3.as_ptr() as u64));
    let page = |addr: u64| (addr - map.ram.0).div_ceil(PAGE);
    let pages = |access| kernel(0, access, VALID_TABLE_OR_PAGE);
    let dtb = (page(map.dtb.start.0), page(map.dtb.end.0));
    fill(l3, (0, dtb.0), map.ram.0, PAGE, pages(Kernel::ReadWrite));
    fill(l3, dtb, map.ram.0, PAGE, pages(Kernel::ReadOnly));
    fill(l3, (dtb.1, 512), map.ram.0, PAGE, pages(Kernel::ReadWrite));
    for guard in map.guards {
        l3[page(guard.0) as usize] = 0;
    }
    // SAFETY: the tables are written and the caller guarantees they map everything in use.
    unsafe { asm!("bl aarch64_mmu_on", out("x9") _, out("x10") _, out("x30") _) }
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

/// Clears the page at `va` in the tables under `l1` and returns its frame; flush the ASID's TLB entries after.
///
/// # Safety
///
/// `va` must be mapped by `map_page` under `l1`, an identity-mapped table that only this address space uses.
pub unsafe fn unmap_page(l1: PhysAddr, va: u64) -> PhysAddr {
    let mut table = l1.0;
    for shift in [30, 21] {
        // SAFETY: the caller guarantees `va` is mapped, so each level holds a table descriptor of this space.
        table = unsafe {
            (table as *const u64)
                .wrapping_add((va >> shift) as usize & 511)
                .read()
        } & ADDR;
    }
    // SAFETY: as above.
    let leaf = unsafe {
        (table as *mut u64)
            .wrapping_add((va >> 12) as usize & 511)
            .replace(0)
    };
    // SAFETY: a barrier only orders the table write before the TLB flush.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    PhysAddr(leaf & ADDR)
}

/// Calls `free` on every frame of the address space under `l1`: its user pages, its level-3 and level-2 tables, then
/// `l1` itself; the kernel's entries, those the boot table holds, are shared and skipped.
///
/// # Safety
///
/// `l1` must be a level-1 table built by `map_page` over a copy of the boot table's entries that no TTBR0 uses any
/// more.
pub unsafe fn free_space(l1: PhysAddr, mut free: impl FnMut(PhysAddr)) {
    // SAFETY: the caller's guarantee.
    unsafe { free_table(l1, 1, &mut free) }
}

/// # Safety
///
/// `table` must be a level-`level` table of an address space built by `map_page`.
unsafe fn free_table<F: FnMut(PhysAddr)>(table: PhysAddr, level: u32, free: &mut F) {
    let boot = &raw const L1;
    for i in 0..512 {
        // SAFETY: the caller guarantees `table` is an identity-mapped table frame.
        let desc = unsafe { (table.0 as *const u64).wrapping_add(i).read() };
        if desc & VALID_TABLE_OR_PAGE != VALID_TABLE_OR_PAGE {
            continue;
        }
        // SAFETY: the boot table is only written by `enable_mmu`, before any address space exists.
        if level == 1 && unsafe { (*boot).0[i] } != 0 {
            continue;
        }
        let next = PhysAddr(desc & ADDR);
        if level == 3 {
            free(next);
        } else {
            // SAFETY: a table descriptor above level 3 points at a table `map_page` added.
            unsafe { free_table(next, level + 1, free) };
        }
    }
    free(table);
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

/// Drops every non-global TLB entry tagged with `asid`, on every core.
pub fn flush_asid(asid: usize) {
    // SAFETY: invalidating TLB entries only forces later walks.
    unsafe {
        asm!(
            "tlbi aside1is, {}",
            "dsb ish",
            "isb",
            in(reg) (asid as u64) << 48,
            options(nostack, preserves_flags)
        )
    };
}

/// Whether EL0 may read `va` in the current address space.
pub fn user_readable(va: u64) -> bool {
    let par: u64;
    // SAFETY: an address translation only writes PAR_EL1, which nothing else reads.
    unsafe {
        asm!(
            "at s1e0r, {va}",
            "isb",
            "mrs {par}, par_el1",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack, preserves_flags),
        )
    };
    par & 1 == 0
}

/// Whether EL0 may write `va` in the current address space.
pub fn user_writable(va: u64) -> bool {
    let par: u64;
    // SAFETY: an address translation only writes PAR_EL1, which nothing else reads.
    unsafe {
        asm!(
            "at s1e0w, {va}",
            "isb",
            "mrs {par}, par_el1",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack, preserves_flags),
        )
    };
    par & 1 == 0
}

/// Cleans `start..start + len` to the point of unification, the first half of making instructions written there
/// visible to instruction fetch; `invalidate_icache` completes it for every range cleaned before it.
///
/// # Safety
///
/// The range must be mapped.
pub unsafe fn clean_dcache(start: usize, len: usize) {
    let ctr: usize;
    // SAFETY: reading CTR_EL0 has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let line = 4 << ((ctr >> 16) & 0xf);
    for addr in (start & !(line - 1)..start + len).step_by(line) {
        // SAFETY: cleaning a mapped line to the point of unification does not change memory contents.
        unsafe { asm!("dc cvau, {}", in(reg) addr, options(nostack, preserves_flags)) };
    }
}

/// Waits for the cleans before it, then discards every stale instruction in every core's I-cache.
pub fn invalidate_icache() {
    // SAFETY: barriers and an I-cache invalidate only discard stale instructions.
    unsafe {
        asm!(
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}
