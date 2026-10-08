use kernel::cpio;
use kernel::elf::Elf;
use kernel::file::list_archive;
use kernel::syscall::EINVAL;

/// A newc archive of `files` (name, data), as `cpio -H newc` writes it.
fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in files.iter().chain([&("TRAILER!!!", &[][..])]) {
        out.extend(b"070701");
        for field in [
            0,
            0o100_755,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0,
        ] {
            out.extend(format!("{field:08x}").bytes());
        }
        out.extend(name.bytes().chain([0]));
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend(*data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

#[test]
fn cpio_finds_files_and_rejects_truncated_entries() {
    let a = archive(&[("ab", b"12345"), ("child", b"xyz")]);
    assert_eq!(cpio::find(&a, b"ab").map(|r| &a[r]), Some(&b"12345"[..]));
    assert_eq!(cpio::find(&a, b"child").map(|r| &a[r]), Some(&b"xyz"[..]));
    assert_eq!(cpio::find(&a, b"a"), None);
    assert_eq!(cpio::find(&a, b"TRAILER!!!"), None);
    // filesize of "ab" claims more than the archive holds.
    let mut huge = a.clone();
    huge[54..62].copy_from_slice(b"ffffffff");
    assert_eq!(cpio::find(&huge, b"child"), None);
    assert_eq!(cpio::find(&a[..a.len() - 20], b"missing"), None);
    assert_eq!(cpio::find(b"070701", b"ab"), None);
}

const REGION: std::ops::Range<u64> = 1 << 32..(1 << 32) + 0x1f_f000;

/// An AArch64 executable entered at 4 GiB with one `PT_LOAD` per (flags, vaddr, file size, memory size), each
/// segment's data at offset 0x1000.
fn elf(segments: &[(u32, u64, u64, u64)]) -> Vec<u8> {
    let mut f = vec![0; 0x2000];
    f[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    f[16..20].copy_from_slice(&[2, 0, 183, 0]);
    f[24..32].copy_from_slice(&REGION.start.to_le_bytes());
    f[32..40].copy_from_slice(&64u64.to_le_bytes());
    f[54..58].copy_from_slice(&[56, 0, segments.len() as u8, 0]);
    for (i, &(flags, vaddr, file_size, size)) in segments.iter().enumerate() {
        let ph = &mut f[64 + 56 * i..120 + 56 * i];
        ph[..4].copy_from_slice(&1u32.to_le_bytes());
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
        ph[16..24].copy_from_slice(&vaddr.to_le_bytes());
        ph[32..40].copy_from_slice(&file_size.to_le_bytes());
        ph[40..48].copy_from_slice(&size.to_le_bytes());
    }
    f
}

#[test]
fn elf_accepts_ordered_segments_in_the_region() {
    let base = REGION.start;
    let f = elf(&[(5, base, 0x1000, 0x1800), (6, base + 0x2000, 0x10, 0x3000)]);
    let elf = Elf::parse(&f, REGION).expect("valid");
    assert_eq!(elf.entry, base);
    let segments: Vec<_> = elf
        .segments()
        .map(|s| (s.vaddr, s.data, s.size, s.writable))
        .collect();
    assert_eq!(
        segments,
        [
            (base, 0x1000..0x2000, 0x1800, false),
            (base + 0x2000, 0x1000..0x1010, 0x3000, true)
        ]
    );
}

#[test]
fn elf_rejects_segments_that_could_harm_the_kernel_or_each_other() {
    let base = REGION.start;
    let rejected = [
        (
            "shares a page",
            elf(&[(5, base, 0x10, 0x1001), (6, base + 0x1000, 0, 0x10)]),
        ),
        (
            "out of order",
            elf(&[(5, base + 0x1000, 0, 0x10), (6, base, 0, 0x10)]),
        ),
        ("below the region", elf(&[(5, base - 0x1000, 0, 0x10)])),
        (
            "past the region",
            elf(&[(5, REGION.end - 0x1000, 0, 0x1001)]),
        ),
        ("wrapping size", elf(&[(5, base, 0, u64::MAX)])),
        ("unaligned", elf(&[(5, base + 8, 0, 0x10)])),
        ("writable and executable", elf(&[(7, base, 0, 0x10)])),
        ("file size over memory size", elf(&[(5, base, 0x20, 0x10)])),
        ("data past the file", elf(&[(5, base, 0x1001, 0x2000)])),
        ("entry in a writable segment", elf(&[(6, base, 0, 0x10)])),
        ("entry in no segment", elf(&[(5, base + 0x1000, 0, 0x10)])),
    ];
    for (why, f) in rejected {
        assert!(Elf::parse(&f, REGION).is_none(), "{why}");
    }
    let mut no_entry = elf(&[(5, base, 0, 0x10)]);
    no_entry[24..32].copy_from_slice(&0x4000_0000u64.to_le_bytes());
    assert!(
        Elf::parse(&no_entry, REGION).is_none(),
        "entry outside the region"
    );
    let mut many = elf(&[]);
    many[56] = 0xff;
    assert!(Elf::parse(&many, REGION).is_none(), "headers past the file");
    assert!(
        Elf::parse(&elf(&[])[..63], REGION).is_none(),
        "short header"
    );
}

#[test]
fn archive_lists_whole_entries() {
    let a = archive(&[("ab", b"12345"), ("child", b"xyz")]);
    let mut out = [0; 9];
    assert_eq!(list_archive(&a, 0, &mut out), Ok((9, u64::MAX)));
    assert_eq!(&out, b"ab\nchild\n");
    assert_eq!(list_archive(&a, 0, &mut out[..8]), Ok((3, 1)));
    assert_eq!(list_archive(&a, 1, &mut out), Ok((6, u64::MAX)));
    assert_eq!(list_archive(&a, 0, &mut out[..2]), Err(EINVAL));
    assert_eq!(list_archive(&a, 2, &mut out), Ok((0, u64::MAX)));
}
