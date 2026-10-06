use dtb::Dtb;
use mm::PhysAddr;

// Dumped with `qemu-system-aarch64 -M virt,dumpdtb=virt.dtb -cpu cortex-a72 -m 128M`.
const VIRT: &[u8] = include_bytes!("virt.dtb");

#[test]
fn reads_total_size_from_header() {
    assert_eq!(dtb::total_size(VIRT), Some(VIRT.len()));
}

#[test]
fn finds_qemu_virt_memory() {
    let memory = Dtb::new(VIRT).unwrap().memory();
    assert_eq!(memory, Some(PhysAddr(0x4000_0000)..PhysAddr(0x4800_0000)));
}

#[test]
fn rejects_bad_or_truncated_blobs() {
    assert_eq!(dtb::total_size(&[0; 8]), None);
    assert_eq!(dtb::total_size(&VIRT[..7]), None);
    assert!(Dtb::new(&VIRT[..VIRT.len() - 1]).is_none());
}

#[test]
fn finds_qemu_virt_uart() {
    assert_eq!(Dtb::new(VIRT).unwrap().uart(), Some(PhysAddr(0x0900_0000)));
}

#[test]
fn no_bootargs_without_append() {
    assert_eq!(Dtb::new(VIRT).unwrap().bootargs(), None);
}
