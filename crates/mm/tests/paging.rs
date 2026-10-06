use mm::{MemoryType, PhysAddr, l1_block};

#[test]
fn encodes_device_block() {
    // Valid block, AttrIndx 0, AF, PXN|UXN.
    assert_eq!(
        l1_block(PhysAddr(0), MemoryType::Device),
        0x0060_0000_0000_0401
    );
}

#[test]
fn encodes_normal_block() {
    // Valid block, AttrIndx 1, inner shareable, AF, executable, address bits [47:30].
    assert_eq!(
        l1_block(PhysAddr(0x4000_0000), MemoryType::Normal),
        0x0000_0000_4000_0705
    );
}
