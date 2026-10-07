//! Speculation: each core picks its vector table as Linux v6.18's `proton-pack.c` decides Spectre-v2, BHB and SSB,
//! and records what it installed for the `spec:` report.

use core::arch::asm;
use core::fmt;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Acquire;

/// Reads the system register `$name`.
macro_rules! sysreg {
    ($name:literal) => {{
        let value: u64;
        // SAFETY: reading an ID register or VBAR_EL1 has no side effects.
        unsafe { asm!(concat!("mrs {}, ", $name), out(reg) value, options(nomem, nostack, preserves_flags)) };
        value
    }};
}

/// How the firmware is called (the DT's PSCI `method`).
#[derive(Clone, Copy, PartialEq)]
pub enum Conduit {
    Hvc,
    Smc,
}

#[derive(Clone, Copy, PartialEq)]
enum Table {
    Plain,
    ClearBhb,
    Firmware(Conduit),
    /// The branch loop `k` times, then `sb` (true) or `dsb nsh; isb`.
    Loop(u8, bool),
}

/// The tables in `aarch64_vectors` order, 2 KiB apart (`trap.rs`).
const TABLES: [Table; 16] = {
    let mut tables = [Table::Plain; 16];
    tables[1] = Table::ClearBhb;
    tables[2] = Table::Firmware(Conduit::Hvc);
    tables[3] = Table::Firmware(Conduit::Smc);
    let mut i = 0;
    while i < LOOP_K.len() {
        tables[4 + 2 * i] = Table::Loop(LOOP_K[i], false);
        tables[5 + 2 * i] = Table::Loop(LOOP_K[i], true);
        i += 1;
    }
    tables
};
/// Linux's loop counts, each with its CPUs; an unlisted CPU gets the largest.
const LOOP_K: [u8; 6] = [8, 11, 24, 32, 38, 132];

const fn model(implementer: u32, part: u32) -> u32 {
    implementer << 24 | 0xf << 16 | part << 4
}
const fn arm(part: u32) -> u32 {
    model(0x41, part)
}
const B53: u32 = model(0x42, 0x100);
const KRYO_2XX_GOLD: u32 = model(0x51, 0x800);
const KRYO_2XX_SILVER: u32 = model(0x51, 0x801);
const KRYO_3XX_SILVER: u32 = model(0x51, 0x803);
const KRYO_4XX_SILVER: u32 = model(0x51, 0x805);
const TSV110: u32 = model(0x48, 0xd01);
const A35: u32 = arm(0xd04);
const A53: u32 = arm(0xd03);
const A55: u32 = arm(0xd05);
const A57: u32 = arm(0xd07);
const A72: u32 = arm(0xd08);
const A73: u32 = arm(0xd09);
const A75: u32 = arm(0xd0a);

const V2_SAFE: &[u32] = &[
    A35,
    A53,
    A55,
    B53,
    TSV110,
    KRYO_2XX_SILVER,
    KRYO_3XX_SILVER,
    KRYO_4XX_SILVER,
];
const BHB_SAFE: &[u32] = &[
    A35,
    A53,
    A55,
    arm(0xd46),
    arm(0xd80),
    B53,
    KRYO_2XX_SILVER,
    KRYO_3XX_SILVER,
    KRYO_4XX_SILVER,
];
const SSB_SAFE: &[u32] = &[A35, A53, A55, B53, KRYO_3XX_SILVER, KRYO_4XX_SILVER];
/// Linux's `kpti_safe_list`.
const MELTDOWN_SAFE: &[u32] = &[
    model(0x43, 0x0af),
    model(0x42, 0x516),
    B53,
    A35,
    A53,
    A55,
    A57,
    A72,
    A73,
    TSV110,
    model(0x4e, 0x004),
    KRYO_2XX_GOLD,
    KRYO_2XX_SILVER,
    KRYO_3XX_SILVER,
    KRYO_4XX_SILVER,
];
/// `spectre_bhb_loop_affected`'s lists, largest k first.
const BHB_K: [(u8, &[u32]); 6] = [
    (132, &[arm(0xd4e), arm(0xd4f)]),
    (38, &[arm(0xd4d), arm(0xd81), arm(0xd89)]),
    (
        32,
        &[
            arm(0xd41),
            arm(0xd42),
            arm(0xd4b),
            arm(0xd44),
            arm(0xd4c),
            arm(0xd47),
            arm(0xd48),
            arm(0xd49),
            arm(0xd40),
        ],
    ),
    (
        24,
        &[
            arm(0xd0b),
            arm(0xd0e),
            arm(0xd0d),
            arm(0xd0c),
            model(0x51, 0x804),
            model(0x48, 0xd02),
        ],
    ),
    (11, &[model(0xc0, 0xac3)]),
    (8, &[A72, A57]),
];

const PSCI_FEATURES: u32 = 0x8400_000a;
const SMCCC_VERSION: u32 = 0x8000_0000;
const ARCH_FEATURES: u32 = 0x8000_0001;
const WORKAROUND_1: u32 = 0x8000_8000;
const WORKAROUND_2: u32 = 0x8000_7fff;
const WORKAROUND_3: u32 = 0x8000_3fff;
const SMCCC_1_1: i64 = 0x1_0001;

// A core's record: fields in severity order, so the worst core's record is the largest; 0 means none yet.
const RECORDED: u32 = 1 << 31;
const BHB: u32 = 24;
const V2: u32 = 20;
const SSB: u32 = 16;
const BSE: u32 = 12;
const MELTDOWN: u32 = 8;
const NOT_AFFECTED: u32 = 0;
const MITIGATED: u32 = 1;
/// v2: firmware workaround 1 exists but is not called yet (phase 11).
const FIRMWARE_UNCALLED: u32 = 1;
const VULNERABLE: u32 = 2;
/// BHB: mitigated by the table; by ECBHB in hardware; by the largest k, as the CPU is unlisted; not mitigated,
/// since v2 is vulnerable.
const BHB_TABLE: u32 = 1;
const BHB_ECBHB: u32 = 2;
const BHB_UNLISTED: u32 = 3;
const BHB_V2: u32 = 4;

fn field(reg: u64, shift: u32) -> u32 {
    ((reg >> shift) & 0xf) as u32
}

fn this_model() -> u32 {
    sysreg!("midr_el1") as u32 & 0xff0f_fff0
}

/// This core's v2 state, BHB state and vector table, in Linux v6.18's order; an ID register is read (a trap under
/// hvf) and the firmware asked only when the decision reaches it.
fn decide(conduit: Option<Conduit>) -> (u32, u32, Table) {
    let model = this_model();
    let on = |list: &[u32]| list.contains(&model);
    let csv2 = field(sysreg!("id_aa64pfr0_el1"), 56);
    let sb = || field(sysreg!("id_aa64isar1_el1"), 36) != 0;
    let v2 = if csv2 != 0 || on(V2_SAFE) {
        NOT_AFFECTED
    } else {
        match workaround(conduit, WORKAROUND_1) {
            0 => FIRMWARE_UNCALLED,
            1 => NOT_AFFECTED,
            _ => VULNERABLE,
        }
    };
    let (bhb, table) = if csv2 == 3 || on(BHB_SAFE) {
        (NOT_AFFECTED, Table::Plain)
    } else if v2 == VULNERABLE {
        (BHB_V2, Table::Plain)
    } else if field(sysreg!("id_aa64mmfr1_el1"), 60) != 0 {
        (BHB_ECBHB, Table::Plain)
    } else if field(sysreg!("s3_0_c0_c6_2"), 28) != 0 {
        (BHB_TABLE, Table::ClearBhb)
    } else if let Some(&(k, _)) = BHB_K.iter().find(|(_, list)| on(list)) {
        (BHB_TABLE, Table::Loop(k, sb()))
    } else if let (Some(conduit), 0) = (conduit, workaround(conduit, WORKAROUND_3)) {
        (BHB_TABLE, Table::Firmware(conduit))
    } else {
        (BHB_UNLISTED, Table::Loop(132, sb()))
    };
    (v2, bhb, table)
}

/// Picks this core's vector table from its own ID registers and the firmware behind `conduit` and writes `VBAR_EL1`,
/// once; clears PSTATE.SSBS where FEAT_SSBS exists. `record_speculation` follows, off the boot path.
pub fn install_vectors(conduit: Option<Conduit>) {
    let (_, _, table) = decide(conduit);
    let index = TABLES.iter().position(|&t| t == table).unwrap();
    // SAFETY: each table is a complete, 2 KiB aligned EL1 vector table, and the tables lie 2 KiB apart from
    // `aarch64_vectors` in `TABLES` order.
    unsafe {
        asm!(
            "adrp {t}, aarch64_vectors",
            "add {t}, {t}, :lo12:aarch64_vectors",
            "add {t}, {t}, {offset}",
            "msr vbar_el1, {t}",
            "isb",
            t = out(reg) _,
            offset = in(reg) index * 2048,
        )
    };
    if field(sysreg!("id_aa64pfr1_el1"), 4) != 0 {
        // SAFETY: `msr ssbs, #0`, which FEAT_SSBS defines; DSSBS = 0 in SCTLR_EL1 clears it on every later entry.
        unsafe { asm!(".inst 0xd503403f", options(nomem, nostack, preserves_flags)) };
    }
}

/// This core's record for `speculation`: the table read back from `VBAR_EL1`, and v2, BHB, SSB, Meltdown and BSE as
/// `install_vectors` decided them. Call after `install_vectors`; never 0.
pub fn record_speculation(conduit: Option<Conduit>) -> u32 {
    let (v2, bhb, _) = decide(conduit);
    let installed = (sysreg!("vbar_el1") - vectors()) / 2048;
    let midr = sysreg!("midr_el1") as u32;
    let model = this_model();
    let on = |list: &[u32]| list.contains(&model);
    let ssb = if on(SSB_SAFE) {
        NOT_AFFECTED
    } else if field(sysreg!("id_aa64pfr1_el1"), 4) != 0 {
        MITIGATED
    } else if matches!(workaround(conduit, WORKAROUND_2), 1 | -2) {
        NOT_AFFECTED
    } else {
        VULNERABLE
    };
    let meltdown = match field(sysreg!("id_aa64pfr0_el1"), 60) != 0 || on(MELTDOWN_SAFE) {
        true => NOT_AFFECTED,
        false => VULNERABLE,
    };
    let bse_affected = [A57, A73, A75].contains(&model) || model == A72 && (midr >> 20) & 0xf == 0;
    let bse = match (bse_affected, TABLES[installed as usize]) {
        (false, _) => NOT_AFFECTED,
        (true, Table::Firmware(_)) => MITIGATED,
        (true, _) => VULNERABLE,
    };
    RECORDED
        | bhb << BHB
        | v2 << V2
        | ssb << SSB
        | bse << BSE
        | meltdown << MELTDOWN
        | installed as u32
}

/// `ARCH_FEATURES` for `workaround`, or -1 (not supported) without SMCCC 1.1, as Linux's `arm_smccc_1_1_get_conduit`
/// decides.
fn workaround(conduit: Option<Conduit>, workaround: u32) -> i64 {
    let Some(c) = conduit else { return -1 };
    if smccc(c, PSCI_FEATURES, SMCCC_VERSION) < 0 || smccc(c, SMCCC_VERSION, 0) < SMCCC_1_1 {
        return -1;
    }
    smccc(c, ARCH_FEATURES, workaround)
}

/// A firmware call; SMCCC 1.0 may clobber x0-x17.
fn smccc(conduit: Conduit, function: u32, arg: u32) -> i64 {
    let result: i64;
    match conduit {
        // SAFETY: PSCI and SMCCC discovery calls only return values.
        Conduit::Hvc => unsafe {
            asm!("hvc #0", inlateout("x0") function as u64 => result, in("x1") arg as u64, clobber_abi("C"))
        },
        // SAFETY: as above.
        Conduit::Smc => unsafe {
            asm!("smc #0", inlateout("x0") function as u64 => result, in("x1") arg as u64, clobber_abi("C"))
        },
    }
    // SMCCC returns 32-bit values.
    result as i32 as i64
}

/// Each of `values` if at most its max, else the max, by `cmp` and `csel`, then one `csdb`: no later instruction uses
/// a value a mispredicted check let through.
#[inline(always)]
pub fn clamp<const N: usize>(values: [u64; N], maxes: [u64; N]) -> [u64; N] {
    let mut out = values;
    for i in 0..N {
        // SAFETY: arithmetic only; not `pure`, so it stays before the `csdb` below.
        unsafe {
            asm!(
                "cmp {v}, {m}",
                "csel {v}, {v}, {m}, ls",
                v = inout(reg) out[i],
                m = in(reg) maxes[i],
                options(nomem, nostack),
            )
        };
    }
    // SAFETY: `csdb` only; no `nomem`, so no load the compiler emits moves above it.
    unsafe { asm!("hint #20", options(nostack, preserves_flags)) };
    out
}

/// `value & mask`, which the compiler cannot see through: a bound by construction it never drops.
#[inline(always)]
pub fn mask(value: u64, mask: u64) -> u64 {
    let masked: u64;
    // SAFETY: arithmetic only.
    unsafe {
        asm!("and {o}, {v}, {m}", o = lateout(reg) masked, v = in(reg) value, m = in(reg) mask, options(pure, nomem, nostack, preserves_flags))
    };
    masked
}

fn vectors() -> u64 {
    let base: u64;
    // SAFETY: an address computation only.
    unsafe {
        asm!("adrp {t}, aarch64_vectors", "add {t}, {t}, :lo12:aarch64_vectors", t = out(reg) base, options(nomem, nostack, preserves_flags))
    };
    base
}

/// The worst of `records` (one per core, 0 until its core records) and how many cores share it, once each recorded.
pub fn speculation(records: &[AtomicU32]) -> Option<Speculation> {
    let cpus = records.len();
    let record = records.iter().map(|r| r.load(Acquire)).max()?;
    if records.iter().any(|r| r.load(Acquire) == 0) {
        return None;
    }
    let same = records.iter().filter(|r| r.load(Acquire) == record).count();
    Some(Speculation { record, same, cpus })
}

/// The `spec:` line's text.
pub struct Speculation {
    record: u32,
    same: usize,
    cpus: usize,
}

impl fmt::Display for Speculation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = |shift| match (self.record >> shift) & 0xf {
            NOT_AFFECTED => "not affected",
            MITIGATED => "mitigated",
            _ => "vulnerable",
        };
        let v2 = match (self.record >> V2) & 0xf {
            FIRMWARE_UNCALLED => "vulnerable (firmware workaround 1 not called)",
            _ => state(V2),
        };
        let bhb = match (self.record >> BHB) & 0xf {
            NOT_AFFECTED => "not affected",
            BHB_TABLE => "mitigated",
            BHB_ECBHB => "mitigated (ecbhb)",
            BHB_UNLISTED => "mitigated (unlisted cpu, largest k)",
            _ => "not mitigated (v2 vulnerable)",
        };
        write!(
            f,
            "v1 mitigated, v2 {v2}, bhb {bhb}, ssb {}, meltdown {}, bse {}, table ",
            state(SSB),
            state(MELTDOWN),
            state(BSE),
        )?;
        match TABLES[(self.record & 0xff) as usize] {
            Table::Plain => write!(f, "plain")?,
            Table::ClearBhb => write!(f, "clearbhb")?,
            Table::Firmware(Conduit::Hvc) => write!(f, "fw3-hvc")?,
            Table::Firmware(Conduit::Smc) => write!(f, "fw3-smc")?,
            Table::Loop(k, true) => write!(f, "loop{k}-sb")?,
            Table::Loop(k, false) => write!(f, "loop{k}-dsb")?,
        }
        write!(f, " on {}/{} cores", self.same, self.cpus)
    }
}
