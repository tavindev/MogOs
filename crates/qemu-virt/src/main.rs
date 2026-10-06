#![no_std]
#![no_main]

mod uart;

use core::fmt::Write;
use core::panic::PanicInfo;
use core::slice;

use core::ops::Range;

use dtb::Dtb;
use linked_list_allocator::LockedHeap;
use mm::{MemoryType, PhysAddr, l1_block};
use uart::Uart;

/// Panic-path console; normal output uses the PL011 from the DTB.
const UART0: usize = 0x0900_0000;
/// QEMU loads the DTB at RAM base for an ELF kernel (x0 stays 0), if it fits below the image.
const DTB: usize = 0x4000_0000;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const GIB: u64 = 1 << 30;
/// Outside both mapped GiBs.
const UNMAPPED: usize = 0x8000_0000;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

core::arch::global_asm!(
    r#"
.section .text.boot
.global _start
_start:
    // Rust for this target emits FP/SIMD; stop CPACR_EL1.FPEN from trapping it.
    mov x1, #(3 << 20)
    msr cpacr_el1, x1
    isb
    ldr x1, =__stack_top
    mov sp, x1
    ldr x1, =__bss_start
    ldr x2, =__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:  bl kmain
3:  wfe
    b 3b
"#
);

struct QemuVirt {
    uart: Uart,
    entry_us: u64,
}

impl kernel::Board for QemuVirt {
    type Console = Uart;

    fn console(&mut self) -> &mut Uart {
        &mut self.uart
    }

    fn exception_level(&self) -> u8 {
        let el: u64;
        // SAFETY: reading CurrentEL has no side effects.
        unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
        (el >> 2) as u8
    }

    fn breakpoint_self_test(&mut self) -> bool {
        aarch64::breakpoint_self_test()
    }

    fn enable_mmu(&mut self) {
        let l1 = [
            l1_block(PhysAddr(0), MemoryType::Device),
            l1_block(PhysAddr(GIB), MemoryType::Normal),
        ];
        // SAFETY: called once at boot before any atomic RMW; MMIO is in GiB 0, and the image, stack and DTB are in RAM in GiB 1.
        unsafe { aarch64::enable_mmu(&l1, mm::MAIR) }
    }

    fn read_unmapped(&mut self) {
        // SAFETY: the address is unmapped, so the read takes a data abort, which panics instead of returning.
        unsafe { (UNMAPPED as *const u64).read_volatile() };
    }

    fn init_heap(&mut self, region: Range<PhysAddr>) {
        let size = (region.end.0 - region.start.0) as usize;
        // SAFETY: the kernel took `region` from its frame allocator, so it is unused, mapped RAM, and init runs once.
        unsafe { HEAP.lock().init(region.start.0 as *mut u8, size) }
    }

    fn uptime_us(&self) -> u64 {
        aarch64::uptime_us() - self.entry_us
    }

    fn power_off(&mut self) -> ! {
        shutdown()
    }
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let entry_us = aarch64::uptime_us();
    aarch64::install_vectors();

    // SAFETY: RAM base is mapped RAM; we only read the 8-byte FDT header there.
    let header = unsafe { slice::from_raw_parts(DTB as *const u8, 8) };
    let size = dtb::total_size(header).expect("no DTB at RAM base");
    // SAFETY: the magic matched, so QEMU loaded `size` bytes of DTB here and nothing writes them.
    let blob = unsafe { slice::from_raw_parts(DTB as *const u8, size) };

    let image =
        PhysAddr(&raw const __kernel_start as u64)..PhysAddr(&raw const __kernel_end as u64);
    let dtb = PhysAddr(DTB as u64)..PhysAddr((DTB + size) as u64);

    let uart = Dtb::new(blob)
        .and_then(|d| d.uart())
        .expect("no PL011 in DTB");

    kernel::run(
        &mut QemuVirt {
            uart: Uart::new(uart.0 as usize),
            entry_us,
        },
        blob,
        &[image, dtb],
    )
}

fn shutdown() -> ! {
    // SAFETY: PSCI SYSTEM_OFF via HVC is the power-off call on QEMU `virt` at EL1.
    unsafe { core::arch::asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(noreturn)) }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(Uart::new(UART0), "panic: {info}");
    shutdown()
}
