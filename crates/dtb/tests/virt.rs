#[test]
fn rejects_bad_or_truncated_blobs() {
    assert_eq!(dtb::total_size(&[0; 8]), None);
    assert_eq!(dtb::total_size(&[0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0]), None);
}

/// QEMU 9.2 `virt` (`-cpu cortex-a72`, `-machine dumpdtb`), recompiled by `dtc` to drop the 1 MiB padding.
static VIRT: &[u8] = include_bytes!("virt.dtb");

#[test]
fn lists_the_32_virtio_mmio_transports_in_ascending_order() {
    let dtb = dtb::Dtb::new(VIRT).unwrap();
    let mut bases = Vec::new();
    dtb.virtio_mmio(|base| {
        bases.push(base.0);
        None::<()>
    });
    let expected: Vec<u64> = (0..32).map(|i| 0x0a00_0000 + i * 0x200).collect();
    assert_eq!(bases, expected);
}
