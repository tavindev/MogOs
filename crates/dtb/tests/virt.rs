#[test]
fn rejects_bad_or_truncated_blobs() {
    assert_eq!(dtb::total_size(&[0; 8]), None);
    assert_eq!(dtb::total_size(&[0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0]), None);
}
