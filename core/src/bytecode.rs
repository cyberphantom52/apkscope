//! Dalvik instruction decoder. Spec: https://source.android.com/docs/core/runtime/dalvik-bytecode
//! and https://source.android.com/docs/core/runtime/instruction-formats

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fmt {
    F10x,
    F12x,
    F11n,
    F11x,
    F10t,
    F20t,
    F22x,
    F21t,
    F21s,
    F21h,
    F21c,
    F23x,
    F22b,
    F22t,
    F22s,
    F22c,
    F30t,
    F32x,
    F31i,
    F31t,
    F31c,
    F35c,
    F3rc,
    F45cc,
    F4rcc,
    F51l,
}
use Fmt::*;

/// What `Insn::idx` points into.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ref {
    None,
    String,
    Type,
    Field,
    Method,
    Proto,
    CallSite,
    MethodHandle,
}

#[derive(Debug, PartialEq)]
pub struct Insn {
    /// offset in code units from method start
    pub off: u32,
    pub op: u8,
    /// registers; for /range formats regs = [first] and nregs = count
    pub regs: [u16; 5],
    pub nregs: u8,
    pub range: bool,
    /// literal value, or branch offset (relative to `off`) for *t formats
    pub lit: i64,
    pub idx: u32,
    /// proto index of invoke-polymorphic
    pub idx2: u32,
}

const BINOPS: [&str; 11] = [
    "add", "sub", "mul", "div", "rem", "and", "or", "xor", "shl", "shr", "ushr",
];
const KINDS: [&str; 7] = [
    "", "-wide", "-object", "-boolean", "-byte", "-char", "-short",
];

/// mnemonic, format, index kind. None = unused opcode. Table built once: decoding is hot.
pub fn info(op: u8) -> Option<(&'static str, Fmt, Ref)> {
    static TABLE: std::sync::OnceLock<Vec<Option<(String, Fmt, Ref)>>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| (0..=255u8).map(build_info).collect())[op as usize]
        .as_ref()
        .map(|(n, f, r)| (n.as_str(), *f, *r))
}

fn build_info(op: u8) -> Option<(String, Fmt, Ref)> {
    let s = |n: &str, f, r| Some((n.to_string(), f, r));
    let n = Ref::None;
    match op {
        0x00 => s("nop", F10x, n),
        0x01 => s("move", F12x, n),
        0x02 => s("move/from16", F22x, n),
        0x03 => s("move/16", F32x, n),
        0x04 => s("move-wide", F12x, n),
        0x05 => s("move-wide/from16", F22x, n),
        0x06 => s("move-wide/16", F32x, n),
        0x07 => s("move-object", F12x, n),
        0x08 => s("move-object/from16", F22x, n),
        0x09 => s("move-object/16", F32x, n),
        0x0a => s("move-result", F11x, n),
        0x0b => s("move-result-wide", F11x, n),
        0x0c => s("move-result-object", F11x, n),
        0x0d => s("move-exception", F11x, n),
        0x0e => s("return-void", F10x, n),
        0x0f => s("return", F11x, n),
        0x10 => s("return-wide", F11x, n),
        0x11 => s("return-object", F11x, n),
        0x12 => s("const/4", F11n, n),
        0x13 => s("const/16", F21s, n),
        0x14 => s("const", F31i, n),
        0x15 => s("const/high16", F21h, n),
        0x16 => s("const-wide/16", F21s, n),
        0x17 => s("const-wide/32", F31i, n),
        0x18 => s("const-wide", F51l, n),
        0x19 => s("const-wide/high16", F21h, n),
        0x1a => s("const-string", F21c, Ref::String),
        0x1b => s("const-string/jumbo", F31c, Ref::String),
        0x1c => s("const-class", F21c, Ref::Type),
        0x1d => s("monitor-enter", F11x, n),
        0x1e => s("monitor-exit", F11x, n),
        0x1f => s("check-cast", F21c, Ref::Type),
        0x20 => s("instance-of", F22c, Ref::Type),
        0x21 => s("array-length", F12x, n),
        0x22 => s("new-instance", F21c, Ref::Type),
        0x23 => s("new-array", F22c, Ref::Type),
        0x24 => s("filled-new-array", F35c, Ref::Type),
        0x25 => s("filled-new-array/range", F3rc, Ref::Type),
        0x26 => s("fill-array-data", F31t, n),
        0x27 => s("throw", F11x, n),
        0x28 => s("goto", F10t, n),
        0x29 => s("goto/16", F20t, n),
        0x2a => s("goto/32", F30t, n),
        0x2b => s("packed-switch", F31t, n),
        0x2c => s("sparse-switch", F31t, n),
        0x2d..=0x31 => s(
            [
                "cmpl-float",
                "cmpg-float",
                "cmpl-double",
                "cmpg-double",
                "cmp-long",
            ][(op - 0x2d) as usize],
            F23x,
            n,
        ),
        0x32..=0x37 => s(
            &format!(
                "if-{}",
                ["eq", "ne", "lt", "ge", "gt", "le"][(op - 0x32) as usize]
            ),
            F22t,
            n,
        ),
        0x38..=0x3d => s(
            &format!(
                "if-{}z",
                ["eq", "ne", "lt", "ge", "gt", "le"][(op - 0x38) as usize]
            ),
            F21t,
            n,
        ),
        0x44..=0x51 => {
            let i = (op - 0x44) as usize;
            s(
                &format!("{}{}", ["aget", "aput"][i / 7], KINDS[i % 7]),
                F23x,
                n,
            )
        }
        0x52..=0x5f => {
            let i = (op - 0x52) as usize;
            s(
                &format!("{}{}", ["iget", "iput"][i / 7], KINDS[i % 7]),
                F22c,
                Ref::Field,
            )
        }
        0x60..=0x6d => {
            let i = (op - 0x60) as usize;
            s(
                &format!("{}{}", ["sget", "sput"][i / 7], KINDS[i % 7]),
                F21c,
                Ref::Field,
            )
        }
        0x6e..=0x72 => s(
            &format!("invoke-{}", INVOKES[(op - 0x6e) as usize]),
            F35c,
            Ref::Method,
        ),
        0x74..=0x78 => s(
            &format!("invoke-{}/range", INVOKES[(op - 0x74) as usize]),
            F3rc,
            Ref::Method,
        ),
        0x7b..=0x8f => s(UNOPS[(op - 0x7b) as usize], F12x, n),
        0x90..=0xcf => {
            let i = ((op - 0x90) % 32) as usize;
            let (name, ty) = if i < 22 {
                (BINOPS[i % 11], ["int", "long"][i / 11])
            } else {
                (BINOPS[(i - 22) % 5], ["float", "double"][(i - 22) / 5])
            };
            let suffix = if op >= 0xb0 { "/2addr" } else { "" };
            s(
                &format!("{name}-{ty}{suffix}"),
                if op >= 0xb0 { F12x } else { F23x },
                n,
            )
        }
        0xd0..=0xd7 => s(
            &format!("{}-int/lit16", LIT_OPS[(op - 0xd0) as usize]),
            F22s,
            n,
        ),
        0xd8..=0xe2 => s(
            &format!("{}-int/lit8", LIT_OPS[(op - 0xd8) as usize]),
            F22b,
            n,
        ),
        0xfa => s("invoke-polymorphic", F45cc, Ref::Method),
        0xfb => s("invoke-polymorphic/range", F4rcc, Ref::Method),
        0xfc => s("invoke-custom", F35c, Ref::CallSite),
        0xfd => s("invoke-custom/range", F3rc, Ref::CallSite),
        0xfe => s("const-method-handle", F21c, Ref::MethodHandle),
        0xff => s("const-method-type", F21c, Ref::Proto),
        _ => None,
    }
}

const INVOKES: [&str; 5] = ["virtual", "super", "direct", "static", "interface"];
const LIT_OPS: [&str; 11] = [
    "add", "rsub", "mul", "div", "rem", "and", "or", "xor", "shl", "shr", "ushr",
];
const UNOPS: [&str; 21] = [
    "neg-int",
    "not-int",
    "neg-long",
    "not-long",
    "neg-float",
    "neg-double",
    "int-to-long",
    "int-to-float",
    "int-to-double",
    "long-to-int",
    "long-to-float",
    "long-to-double",
    "float-to-int",
    "float-to-long",
    "float-to-double",
    "double-to-int",
    "double-to-long",
    "double-to-float",
    "int-to-byte",
    "int-to-char",
    "int-to-short",
];

fn size(f: Fmt) -> usize {
    match f {
        F10x | F12x | F11n | F11x | F10t => 1,
        F30t | F32x | F31i | F31t | F31c | F35c | F3rc => 3,
        F45cc | F4rcc => 4,
        F51l => 5,
        _ => 2,
    }
}

/// Code units of an instruction in format `f`.
pub fn width(f: Fmt) -> usize {
    match f {
        F10x | F12x | F11n | F11x | F10t => 1,
        F20t | F22x | F21t | F21s | F21h | F21c | F23x | F22b | F22t | F22s | F22c => 2,
        F30t | F32x | F31i | F31t | F31c | F35c | F3rc => 3,
        F45cc | F4rcc => 4,
        F51l => 5,
    }
}

/// Every (kind, index) a method body refers to, walking instruction widths without decoding
/// operands or allocating: the hot loop of xref scans. Stops at the first malformed unit.
pub fn for_each_ref(insns: &[u8], mut f: impl FnMut(Ref, u32)) {
    static TABLE: std::sync::OnceLock<Vec<(u8, Ref, bool)>> = std::sync::OnceLock::new();
    // (width, ref kind, 32-bit index); width 0 = unused opcode
    let table = TABLE.get_or_init(|| {
        (0..=255u8)
            .map(|op| {
                info(op).map_or((0, Ref::None, false), |(_, fm, r)| {
                    (width(fm) as u8, r, fm == F31c)
                })
            })
            .collect()
    });
    let unit = |pc: usize| {
        insns
            .get(pc * 2..pc * 2 + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let n = insns.len() / 2;
    let mut pc = 0;
    while pc < n {
        let Some(w0) = unit(pc) else { return };
        let op = w0 & 0xff;
        if op == 0 && w0 != 0 {
            let (Some(a), Some(b), Some(c)) = (
                unit(pc + 1),
                unit(pc + 2).or(Some(0)),
                unit(pc + 3).or(Some(0)),
            ) else {
                return;
            };
            pc += match w0 {
                0x0100 => 4 + a * 2,
                0x0200 => 2 + a * 4,
                0x0300 => 4 + ((b | c << 16) * a).div_ceil(2),
                _ => return,
            };
            continue;
        }
        let (w, r, wide) = table[op];
        if w == 0 {
            return;
        }
        if r != Ref::None {
            let Some(lo) = unit(pc + 1) else { return };
            let idx = if wide {
                lo | unit(pc + 2).unwrap_or(0) << 16
            } else {
                lo
            };
            f(r, idx as u32);
        }
        pc += w as usize;
    }
}

/// Decode a whole method body. `insns` is little-endian code units.
/// Switch / array payload pseudo-instructions are skipped, not returned.
pub fn decode(insns: &[u8]) -> Result<Vec<Insn>, String> {
    let w: Vec<u16> = insns
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut out = Vec::with_capacity(w.len() / 2);
    let mut pc = 0;
    while pc < w.len() {
        let u = |i: usize| {
            w.get(pc + i)
                .copied()
                .map(u32::from)
                .ok_or_else(|| format!("truncated insn at {pc:#x}"))
        };
        let w0 = u(0)?;
        let op = (w0 & 0xff) as u8;
        // payloads hide behind nop with a non-zero high byte
        if op == 0 && w0 != 0 {
            pc += match w0 {
                0x0100 => 4 + u(1)? as usize * 2,
                0x0200 => 2 + u(1)? as usize * 4,
                0x0300 => 4 + ((u(2)? | u(3)? << 16) as usize * u(1)? as usize).div_ceil(2),
                _ => return Err(format!("bad payload {w0:#x} at {pc:#x}")),
            };
            continue;
        }
        let (_, f, _) = info(op).ok_or_else(|| format!("unused opcode {op:#x} at {pc:#x}"))?;
        let mut i = Insn {
            off: pc as u32,
            op,
            regs: [0; 5],
            nregs: 0,
            range: false,
            lit: 0,
            idx: 0,
            idx2: 0,
        };
        let a8 = (w0 >> 8) as u16;
        let (a4, b4) = (((w0 >> 8) & 0xf) as u16, (w0 >> 12) as u16);
        let mut set = |r: &[u16]| {
            i.regs[..r.len()].copy_from_slice(r);
            i.nregs = r.len() as u8;
        };
        match f {
            F10x => {}
            F12x => set(&[a4, b4]),
            F11n => {
                set(&[a4]);
                i.lit = ((w0 as i32) << 16 >> 28) as i64
            }
            F11x => set(&[a8]),
            F10t => i.lit = (w0 >> 8) as u8 as i8 as i64,
            F20t => i.lit = u(1)? as u16 as i16 as i64,
            F22x => set(&[a8, u(1)? as u16]),
            F21t | F21s => {
                set(&[a8]);
                i.lit = u(1)? as u16 as i16 as i64
            }
            F21h => {
                set(&[a8]);
                i.lit = (u(1)? as u16 as i16 as i64) << if op == 0x19 { 48 } else { 16 }
            }
            F21c => {
                set(&[a8]);
                i.idx = u(1)?
            }
            F23x => {
                let x = u(1)? as u16;
                set(&[a8, x & 0xff, x >> 8])
            }
            F22b => {
                let x = u(1)?;
                set(&[a8, (x & 0xff) as u16]);
                i.lit = (x >> 8) as u8 as i8 as i64
            }
            F22t | F22s => {
                set(&[a4, b4]);
                i.lit = u(1)? as u16 as i16 as i64
            }
            F22c => {
                set(&[a4, b4]);
                i.idx = u(1)?
            }
            F30t => i.lit = (u(1)? | u(2)? << 16) as i32 as i64,
            F32x => set(&[u(1)? as u16, u(2)? as u16]),
            F31i | F31t => {
                set(&[a8]);
                i.lit = (u(1)? | u(2)? << 16) as i32 as i64
            }
            F31c => {
                set(&[a8]);
                i.idx = u(1)? | u(2)? << 16
            }
            F35c | F45cc => {
                let x = u(2)? as u16;
                let all = [x & 0xf, (x >> 4) & 0xf, (x >> 8) & 0xf, x >> 12, a4];
                let n = b4 as usize;
                if n > 5 {
                    return Err(format!("bad arg count at {pc:#x}"));
                }
                set(&all[..n]);
                i.idx = u(1)?;
                if f == F45cc {
                    i.idx2 = u(3)?
                }
            }
            F3rc | F4rcc => {
                i.regs[0] = u(2)? as u16;
                i.nregs = a8 as u8;
                i.range = true;
                i.idx = u(1)?;
                if f == F4rcc {
                    i.idx2 = u(3)?
                }
            }
            F51l => {
                set(&[a8]);
                i.lit = (u(1)? | u(2)? << 16) as i64 | ((u(3)? | u(4)? << 16) as i64) << 32
            }
        }
        out.push(i);
        pc += size(f);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn decodes_common_forms() {
        // const/4 v1, -1 ; invoke-virtual {v1, v2}, meth@7 ; if-eqz v0, -2 ; packed-switch payload ; return-void
        let code = enc(&[
            0xf112, 0x206e, 0x0007, 0x0021, 0x0038, 0xfffe, 0x0100, 0x0001, 0, 0, 0, 0, 0x000e,
        ]);
        let v = decode(&code).unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!((v[0].op, v[0].regs[0], v[0].lit), (0x12, 1, -1));
        assert_eq!(
            (v[1].nregs, &v[1].regs[..2], v[1].idx),
            (2, &[1u16, 2][..], 7)
        );
        assert_eq!((v[2].off, v[2].lit), (4, -2));
        assert_eq!((v[3].op, v[3].off), (0x0e, 12));
    }

    #[test]
    fn every_used_opcode_has_name() {
        let used = (0..=255u8).filter(|&o| info(o).is_some()).count();
        assert_eq!(used, 256 - 6 - 1 - 2 - 23); // 3e-43, 73, 79-7a, e3-f9 unused
        assert_eq!(info(0xb0).unwrap().0, "add-int/2addr");
        assert_eq!(info(0xcf).unwrap().0, "rem-double/2addr");
        assert_eq!(info(0xaf).unwrap().0, "rem-double");
        assert_eq!(info(0x5a).unwrap().0, "iput-wide");
    }
}
