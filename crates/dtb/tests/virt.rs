#[test]
fn rejects_bad_or_truncated_blobs() {
    assert_eq!(dtb::total_size(&[0; 8]), None);
    assert_eq!(dtb::total_size(&[0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0]), None);
}

/// QEMU 9.2.1's `-M virt,gic-version=3 -smp 128` blob (`dumpdtb`), its padding cut.
const VIRT_128: &[u8] = include_bytes!("virt-gicv3-128.dtb");

#[test]
fn reads_every_cpus_mpidr_in_order_and_both_redistributor_regions() {
    let dtb = dtb::Dtb::new(VIRT_128).unwrap();
    let mut mpidrs = Vec::new();
    assert_eq!(dtb.cpus(|m| mpidrs.push(m)), 128);
    // 16 cores per Aff1 cluster.
    assert_eq!(
        (mpidrs[0], mpidrs[15], mpidrs[16], mpidrs[127]),
        (0, 15, 0x100, 0x70f)
    );
    let gic = dtb.gic().unwrap();
    assert_eq!(gic.distributor(), Some(mm::PhysAddr(0x0800_0000)));
    let regions: Vec<_> = gic.redistributors().collect();
    // 123 frames of 128 KiB in GiB 0, the rest at 256 GiB.
    assert_eq!(regions[0], (mm::PhysAddr(0x080a_0000), 123 * 0x2_0000));
    assert_eq!(regions[1].0, mm::PhysAddr(256 << 30));
    assert!(regions[1].1 >= 5 * 0x2_0000 && regions.len() == 2);
}
