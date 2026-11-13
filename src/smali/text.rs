//! Class model <-> Smali text, in baksmali syntax (what apktool prints), with p-registers and
//! `.locals`. The printer's output parses back to the same model.

use super::model::*;
use crate::bytecode::{self, Fmt};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::sync::Arc;

type R<T> = Result<T, String>;

// ---------- printing ----------

const CLASS_FLAGS: &[(u32, &str)] = &[
    (0x1, "public"),
    (0x2, "private"),
    (0x4, "protected"),
    (0x8, "static"),
    (0x10, "final"),
    (0x200, "interface"),
    (0x400, "abstract"),
    (0x1000, "synthetic"),
    (0x2000, "annotation"),
    (0x4000, "enum"),
];
const FIELD_FLAGS: &[(u32, &str)] = &[
    (0x1, "public"),
    (0x2, "private"),
    (0x4, "protected"),
    (0x8, "static"),
    (0x10, "final"),
    (0x40, "volatile"),
    (0x80, "transient"),
    (0x1000, "synthetic"),
    (0x4000, "enum"),
];
const METHOD_FLAGS: &[(u32, &str)] = &[
    (0x1, "public"),
    (0x2, "private"),
    (0x4, "protected"),
    (0x8, "static"),
    (0x10, "final"),
    (0x20, "synchronized"),
    (0x40, "bridge"),
    (0x80, "varargs"),
    (0x100, "native"),
    (0x400, "abstract"),
    (0x800, "strictfp"),
    (0x1000, "synthetic"),
    (0x10000, "constructor"),
    (0x20000, "declared-synchronized"),
];
/// hiddenapi flags as ART defines them: the low four bits are the API list, the bits above mark
/// API domains. The list names are baksmali's; `greylist-max-s` continues them.
const HIDDEN_LISTS: [&str; 8] = [
    "whitelist",
    "greylist",
    "blacklist",
    "greylist-max-o",
    "greylist-max-p",
    "greylist-max-q",
    "greylist-max-r",
    "greylist-max-s",
];
const HIDDEN_DOMAINS: &[(u32, &str)] = &[(0x10, "core-platform-api"), (0x20, "test-api")];
/// flags with a list or a domain bit that has no name, kept as a number
const HIDDEN_RAW: &str = "hiddenapi-";
const HANDLE_KINDS: [&str; 9] = [
    "static-put",
    "static-get",
    "instance-put",
    "instance-get",
    "invoke-static",
    "invoke-instance",
    "invoke-constructor",
    "invoke-direct",
    "invoke-interface",
];
const VIS: [&str; 3] = ["build", "runtime", "system"];

fn hidden(out: &mut String, h: Option<u32>) {
    let Some(h) = h else { return };
    match HIDDEN_LISTS.get((h & 0xf) as usize) {
        Some(list) if h >> 6 == 0 => {
            out.push_str(list);
            out.push(' ');
            flags(out, h & !0xf, HIDDEN_DOMAINS);
        }
        _ => {
            let _ = write!(out, "{HIDDEN_RAW}{h:#x} ");
        }
    }
}

fn flags(out: &mut String, a: u32, table: &[(u32, &str)]) {
    let mut left = a;
    for &(bit, name) in table {
        if a & bit != 0 {
            out.push_str(name);
            out.push(' ');
            left &= !bit;
        }
    }
    if left != 0 {
        let _ = write!(out, "{left:#x} ");
    }
}

/// A name or descriptor as text. shortcut: a name holding a lone surrogate prints as U+FFFD;
/// no real app uses one in a class or member name.
fn name(b: &[u8]) -> std::borrow::Cow<'_, str> {
    if b.is_ascii() {
        return std::borrow::Cow::Borrowed(std::str::from_utf8(b).unwrap());
    }
    std::borrow::Cow::Owned(String::from_utf16_lossy(&utf16(b)))
}

fn quote(b: &[u8]) -> String {
    let mut o = String::with_capacity(b.len() + 2);
    o.push('"');
    let units = utf16(b);
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        i += 1;
        match u {
            0x22 => o.push_str("\\\""),
            0x27 => o.push_str("\\'"),
            0x5c => o.push_str("\\\\"),
            0x0a => o.push_str("\\n"),
            0x0d => o.push_str("\\r"),
            0x09 => o.push_str("\\t"),
            0x20..=0x7e => o.push(u as u8 as char),
            0xd800..=0xdbff if i < units.len() && (0xdc00..=0xdfff).contains(&units[i]) => {
                let c = char::decode_utf16([u, units[i]]).next().unwrap().unwrap();
                i += 1;
                o.push(c);
            }
            _ => match char::from_u32(u as u32) {
                Some(c)
                    if !c.is_control()
                        && !c.is_whitespace()
                        && !(0xd800..=0xdfff).contains(&u)
                        && u != 0xfeff =>
                {
                    o.push(c)
                }
                _ => {
                    let _ = write!(o, "\\u{u:04x}");
                }
            },
        }
    }
    o.push('"');
    o
}

fn hex(v: i64) -> String {
    if v < 0 {
        format!("-{:#x}", v.unsigned_abs())
    } else {
        format!("{v:#x}")
    }
}

fn proto(p: &Proto) -> String {
    let mut o = String::from("(");
    p.params.iter().for_each(|t| o.push_str(&name(t)));
    o.push(')');
    o.push_str(&name(&p.ret));
    o
}
fn field_ref(f: &FieldRef) -> String {
    format!("{}->{}:{}", name(&f.class), name(&f.name), name(&f.ty))
}
fn method_ref(m: &MethodRef) -> String {
    format!("{}->{}{}", name(&m.class), name(&m.name), proto(&m.proto))
}
fn handle(h: &Handle) -> String {
    let k = HANDLE_KINDS
        .get(h.kind as usize)
        .map_or_else(|| format!("{:#x}", h.kind), |s| s.to_string());
    match &h.member {
        Member::Field(f) => format!("{k}@{}", field_ref(f)),
        Member::Method(m) => format!("{k}@{}", method_ref(m)),
    }
}

fn float(bits: u32) -> String {
    let f = f32::from_bits(bits);
    // shortcut: NaN payloads other than the canonical one print as NaNf
    if f.is_nan() {
        "NaNf".into()
    } else if f.is_infinite() {
        format!("{}Infinityf", if f < 0.0 { "-" } else { "" })
    } else {
        format!("{f:?}f")
    }
}
fn double(bits: u64) -> String {
    let f = f64::from_bits(bits);
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        format!("{}Infinity", if f < 0.0 { "-" } else { "" })
    } else {
        format!("{f:?}")
    }
}

fn value(o: &mut String, v: &Value, ind: usize) {
    let pad = "    ".repeat(ind);
    match v {
        Value::Byte(x) => *o += &format!("{}t", hex(*x as i64)),
        Value::Short(x) => *o += &format!("{}s", hex(*x as i64)),
        Value::Char(x) => {
            let q = quote(&mutf8(&[*x]));
            let inner = &q[1..q.len() - 1];
            *o += &format!(
                "'{}'",
                if inner == "\\'" {
                    "\\'"
                } else if inner == "\"" {
                    "\""
                } else {
                    inner
                }
            );
        }
        Value::Int(x) => *o += &hex(*x as i64),
        Value::Long(x) => *o += &format!("{}L", hex(*x)),
        Value::Float(x) => *o += &float(*x),
        Value::Double(x) => *o += &double(*x),
        Value::MethodType(p) => *o += &proto(p),
        Value::MethodHandle(h) => *o += &handle(h),
        Value::Str(x) => *o += &quote(x),
        Value::Type(t) => *o += &name(t),
        Value::Field(f) => *o += &field_ref(f),
        Value::Method(m) => *o += &method_ref(m),
        Value::Enum(f) => *o += &format!(".enum {}", field_ref(f)),
        Value::Array(a) if a.is_empty() => *o += "{}",
        Value::Array(a) => {
            *o += "{\n";
            for (k, x) in a.iter().enumerate() {
                *o += &pad;
                *o += "    ";
                value(o, x, ind + 1);
                *o += if k + 1 < a.len() { ",\n" } else { "\n" };
            }
            *o += &pad;
            *o += "}";
        }
        Value::Anno(a) => {
            *o += &format!(".subannotation {}\n", name(&a.ty));
            elems(o, a, ind + 1);
            *o += &pad;
            *o += ".end subannotation";
        }
        Value::Null => *o += "null",
        Value::Bool(b) => *o += if *b { "true" } else { "false" },
    }
}

fn elems(o: &mut String, a: &Anno, ind: usize) {
    for (n, v) in &a.elems {
        *o += &"    ".repeat(ind);
        *o += &format!("{} = ", name(n));
        value(o, v, ind);
        o.push('\n');
    }
}

fn annos(o: &mut String, list: &[Anno], ind: usize) {
    let pad = "    ".repeat(ind);
    for a in list {
        let vis = VIS
            .get(a.vis as usize)
            .map_or_else(|| a.vis.to_string(), |s| s.to_string());
        *o += &format!("{pad}.annotation {vis} {}\n", name(&a.ty));
        elems(o, a, ind + 1);
        *o += &format!("{pad}.end annotation\n");
    }
}

/// Smali of one class.
pub fn print(c: &Class) -> String {
    let mut o = String::with_capacity(4096);
    o += ".class ";
    flags(&mut o, c.access, CLASS_FLAGS);
    o += &name(&c.name);
    o += "\n";
    if let Some(s) = &c.sup {
        o += &format!(".super {}\n", name(s));
    }
    if let Some(s) = &c.source {
        o += &format!(".source {}\n", quote(s));
    }
    for i in &c.ifaces {
        o += &format!(".implements {}\n", name(i));
    }
    if !c.annos.is_empty() {
        o += "\n";
        annos(&mut o, &c.annos, 0);
    }
    for f in &c.fields {
        o += "\n.field ";
        flags(&mut o, f.access, FIELD_FLAGS);
        hidden(&mut o, f.hidden);
        o += &format!("{}:{}", name(&f.name), name(&f.ty));
        if let Some(v) = &f.init {
            o += " = ";
            value(&mut o, v, 0);
        }
        o += "\n";
        if !f.annos.is_empty() {
            annos(&mut o, &f.annos, 1);
            o += ".end field\n";
        }
    }
    for m in &c.methods {
        o += "\n";
        method(&mut o, m);
    }
    o
}

fn label_name(kind: &str, id: u32) -> String {
    format!(":{kind}_{id:x}")
}

fn method(o: &mut String, m: &Method) {
    o.push_str(".method ");
    flags(o, m.access, METHOD_FLAGS);
    hidden(o, m.hidden);
    *o += &format!("{}{}\n", name(&m.name), proto(&m.proto));
    let ins = m.ins();
    let base = m.code.as_ref().map_or(0, |c| c.registers - ins);
    let reg = |r: u16| {
        if r >= base {
            format!("p{}", r - base)
        } else {
            format!("v{r}")
        }
    };
    if let Some(c) = &m.code {
        *o += &format!("    .locals {}\n", c.registers - ins);
    }
    // parameters: debug names and annotations, by p-register
    let mut p = u16::from(m.access & ACC_STATIC == 0);
    for (i, t) in m.proto.params.iter().enumerate() {
        let nm = m
            .code
            .as_ref()
            .and_then(|c| c.param_names.get(i).cloned().flatten());
        let an = m
            .param_annos
            .as_ref()
            .and_then(|v| v.get(i))
            .filter(|v| !v.is_empty());
        if nm.is_some() || an.is_some() {
            *o += &format!("    .param p{p}");
            if let Some(n) = &nm {
                *o += &format!(", {}", quote(n));
            }
            *o += &format!("    # {}\n", name(t));
            if let Some(an) = an {
                annos(o, an, 2);
                *o += "    .end param\n";
            }
        }
        p += width(t);
    }
    annos(o, &m.annos, 1);
    let Some(c) = &m.code else {
        *o += ".end method\n";
        return;
    };

    // label names by how they are used
    let mut kinds: HashMap<u32, BTreeSet<&str>> = HashMap::new();
    let mut add = |id: u32, k: &'static str| {
        kinds.entry(id).or_default().insert(k);
    };
    for it in &c.items {
        match it {
            Item::Ins(i) => match i.op {
                0x28..=0x2a => add(i.target, "goto"),
                0x32..=0x3d => add(i.target, "cond"),
                0x26 => add(i.target, "array"),
                0x2b => add(i.target, "pswitch_data"),
                0x2c => add(i.target, "sswitch_data"),
                _ => {}
            },
            Item::Packed { targets, .. } => targets.iter().for_each(|t| add(*t, "pswitch")),
            Item::Sparse(v) => v.iter().for_each(|t| add(t.1, "sswitch")),
            _ => {}
        }
    }
    for t in &c.tries {
        add(t.start, "try_start");
        add(t.end, "try_end");
        for h in &t.handlers {
            add(h.1, if h.0.is_some() { "catch" } else { "catchall" });
        }
    }
    let lbl = |kind: &str, id: u32| label_name(kind, id);

    for it in &c.items {
        match it {
            Item::Label(id) => {
                if let Some(ks) = kinds.get(id) {
                    o.push('\n');
                    for k in ks {
                        *o += &format!("    {}\n", lbl(k, *id));
                    }
                }
            }
            Item::Ins(i) => {
                *o += "    ";
                print_ins(o, i, &reg, &lbl);
                o.push('\n');
            }
            Item::Packed { first, targets } => {
                *o += &format!("    .packed-switch {}\n", hex(*first as i64));
                for t in targets {
                    *o += &format!("        {}\n", lbl("pswitch", *t));
                }
                *o += "    .end packed-switch\n";
            }
            Item::Sparse(v) => {
                *o += "    .sparse-switch\n";
                for (k, t) in v {
                    *o += &format!("        {} -> {}\n", hex(*k as i64), lbl("sswitch", *t));
                }
                *o += "    .end sparse-switch\n";
            }
            Item::Array { width, data } => {
                *o += &format!("    .array-data {width}\n");
                let w = (*width as usize).max(1);
                let suffix = match w {
                    1 => "t",
                    2 => "s",
                    8 => "L",
                    _ => "",
                };
                for ch in data.chunks(w) {
                    let mut b = [0u8; 8];
                    b[..ch.len()].copy_from_slice(ch);
                    let v = i64::from_le_bytes(b);
                    let v = if w < 8 {
                        (v << (64 - 8 * w)) >> (64 - 8 * w)
                    } else {
                        v
                    };
                    *o += &format!("        {}{suffix}\n", hex(v));
                }
                *o += "    .end array-data\n";
            }
            Item::Line(l) => *o += &format!("    .line {l}\n"),
            Item::Local {
                reg: r,
                name: n,
                ty,
                sig,
            } => {
                *o += &format!(
                    "    .local {}, {}:{}",
                    reg(*r),
                    n.as_ref().map_or("null".into(), |x| quote(x)),
                    ty.as_ref().map_or("null".into(), |x| name(x).into_owned())
                );
                if let Some(s) = sig {
                    *o += &format!(", {}", quote(s));
                }
                o.push('\n');
            }
            Item::EndLocal(r) => *o += &format!("    .end local {}\n", reg(*r)),
            Item::Restart(r) => *o += &format!("    .restart local {}\n", reg(*r)),
            Item::Prologue => *o += "    .prologue\n",
            Item::Epilogue => *o += "    .epilogue\n",
            Item::SetFile(f) => {
                *o += &format!(
                    "    .source {}\n",
                    f.as_ref().map_or("null".into(), |x| quote(x))
                )
            }
        }
    }
    if !c.tries.is_empty() {
        o.push('\n');
    }
    for t in &c.tries {
        for (ty, h) in &t.handlers {
            match ty {
                Some(ty) => {
                    *o += &format!(
                        "    .catch {} {{{} .. {}}} {}\n",
                        name(ty),
                        lbl("try_start", t.start),
                        lbl("try_end", t.end),
                        lbl("catch", *h)
                    )
                }
                None => {
                    *o += &format!(
                        "    .catchall {{{} .. {}}} {}\n",
                        lbl("try_start", t.start),
                        lbl("try_end", t.end),
                        lbl("catchall", *h)
                    )
                }
            }
        }
    }
    *o += ".end method\n";
}

fn print_ins(
    o: &mut String,
    i: &Ins,
    reg: &dyn Fn(u16) -> String,
    lbl: &dyn Fn(&str, u32) -> String,
) {
    let (mn, f, _) = bytecode::info(i.op).unwrap();
    o.push_str(mn);
    let r = |k: usize| reg(i.regs[k]);
    let wide = matches!(i.op, 0x16..=0x19);
    let lit = |v: i64| if wide { format!("{}L", hex(v)) } else { hex(v) };
    let target = || {
        let kind = match i.op {
            0x28..=0x2a => "goto",
            0x26 => "array",
            0x2b => "pswitch_data",
            0x2c => "sswitch_data",
            _ => "cond",
        };
        lbl(kind, i.target)
    };
    let rf = || -> String {
        match &i.r {
            Ref::None => String::new(),
            Ref::Str(x) => quote(x),
            Ref::Type(t) => name(t).into_owned(),
            Ref::Field(x) => field_ref(x),
            Ref::Method(m) => method_ref(m),
            Ref::Proto(p) => proto(p),
            Ref::Handle(h) => handle(h),
            Ref::CallSite(cs) => {
                let mut s = String::from("call_site(");
                for (k, v) in cs.iter().skip(1).enumerate() {
                    if k > 0 {
                        s += ", ";
                    }
                    value(&mut s, v, 0);
                }
                s += ")@";
                match cs.first() {
                    Some(Value::MethodHandle(h)) if h.kind == 4 => {
                        if let Member::Method(m) = &h.member {
                            s += &method_ref(m);
                        }
                    }
                    Some(Value::MethodHandle(h)) => s += &handle(h),
                    _ => s += "?",
                }
                s
            }
        }
    };
    let list = || -> String {
        if f == Fmt::F3rc || f == Fmt::F4rcc {
            if i.n == 0 {
                "{}".into()
            } else {
                format!(
                    "{{{} .. {}}}",
                    reg(i.regs[0]),
                    reg(i.regs[0] + i.n as u16 - 1)
                )
            }
        } else {
            format!(
                "{{{}}}",
                (0..i.n as usize).map(r).collect::<Vec<_>>().join(", ")
            )
        }
    };
    let s = match f {
        Fmt::F10x => String::new(),
        Fmt::F12x | Fmt::F22x | Fmt::F32x => format!(" {}, {}", r(0), r(1)),
        Fmt::F11n | Fmt::F21s | Fmt::F31i | Fmt::F21h | Fmt::F51l => {
            format!(" {}, {}", r(0), lit(i.lit))
        }
        Fmt::F11x => format!(" {}", r(0)),
        Fmt::F10t | Fmt::F20t | Fmt::F30t => format!(" {}", target()),
        Fmt::F21t | Fmt::F31t => format!(" {}, {}", r(0), target()),
        Fmt::F21c | Fmt::F31c => format!(" {}, {}", r(0), rf()),
        Fmt::F23x => format!(" {}, {}, {}", r(0), r(1), r(2)),
        Fmt::F22b | Fmt::F22s => format!(" {}, {}, {}", r(0), r(1), hex(i.lit)),
        Fmt::F22t => format!(" {}, {}, {}", r(0), r(1), target()),
        Fmt::F22c => format!(" {}, {}, {}", r(0), r(1), rf()),
        Fmt::F35c | Fmt::F3rc => format!(" {}, {}", list(), rf()),
        Fmt::F45cc | Fmt::F4rcc => format!(
            " {}, {}, {}",
            list(),
            rf(),
            i.proto.as_ref().map_or(String::new(), proto)
        ),
    };
    o.push_str(&s);
}

// ---------- parsing ----------

fn opcodes() -> &'static HashMap<&'static str, u8> {
    static T: std::sync::OnceLock<HashMap<&'static str, u8>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        (0..=255u8)
            .filter_map(|op| bytecode::info(op).map(|(n, _, _)| (n, op)))
            .collect()
    })
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

fn text_bytes(t: &str) -> S {
    if t.is_ascii() {
        s(t.as_bytes())
    } else {
        Arc::from(mutf8(&t.encode_utf16().collect::<Vec<_>>()))
    }
}

impl<'a> P<'a> {
    fn err<T>(&self, m: impl std::fmt::Display) -> R<T> {
        let line = self.s[..self.i.min(self.s.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        Err(format!("line {line}: {m}"))
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn at(&self, t: &str) -> bool {
        self.s[self.i..].starts_with(t.as_bytes())
    }
    /// spaces, tabs and comments, not newlines
    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'#' => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn wsn(&mut self) {
        loop {
            self.ws();
            if self.peek() == Some(b'\n') {
                self.i += 1;
            } else {
                break;
            }
        }
    }
    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.at(t) {
            self.i += t.len();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &str) -> R<()> {
        if self.eat(t) {
            Ok(())
        } else {
            self.err(format!("expected `{t}`"))
        }
    }
    fn eol(&mut self) -> R<()> {
        self.ws();
        match self.peek() {
            None | Some(b'\n') => Ok(()),
            _ => self.err(format!("unexpected `{}`", self.rest_of_line())),
        }
    }
    fn rest_of_line(&self) -> String {
        let e = self.s[self.i..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.s.len(), |p| self.i + p);
        String::from_utf8_lossy(&self.s[self.i..e])
            .trim()
            .to_string()
    }
    /// a run of non-space characters, stopping at `,` `}` `)` and comments
    fn word(&mut self) -> &'a str {
        self.ws();
        let st = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() || matches!(c, b',' | b'}' | b')' | b'#') {
                break;
            }
            self.i += 1;
        }
        std::str::from_utf8(&self.s[st..self.i]).unwrap_or("")
    }
    fn ty(&mut self) -> R<S> {
        self.ws();
        let st = self.i;
        while self.peek() == Some(b'[') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'L') => {
                let e = self.s[self.i..]
                    .iter()
                    .position(|&b| b == b';' || b == b'\n')
                    .map(|p| self.i + p);
                match e {
                    Some(e) if self.s[e] == b';' => self.i = e + 1,
                    _ => return self.err("class descriptor without `;`"),
                }
            }
            Some(b'V' | b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D') => self.i += 1,
            _ => return self.err(format!("expected a type, found `{}`", self.rest_of_line())),
        }
        Ok(text_bytes(
            std::str::from_utf8(&self.s[st..self.i]).map_err(|e| e.to_string())?,
        ))
    }
    fn proto(&mut self) -> R<Proto> {
        self.expect("(")?;
        let mut params = vec![];
        while !self.eat(")") {
            params.push(self.ty()?);
        }
        Ok(Proto {
            params,
            ret: self.ty()?,
        })
    }
    fn member(&mut self) -> R<Member> {
        let class = self.ty()?;
        self.expect("->")?;
        let st = self.i;
        while let Some(c) = self.peek() {
            if c == b'(' || c == b':' || c.is_ascii_whitespace() {
                break;
            }
            self.i += 1;
        }
        let nm = text_bytes(std::str::from_utf8(&self.s[st..self.i]).map_err(|e| e.to_string())?);
        if self.peek() == Some(b'(') {
            Ok(Member::Method(MethodRef {
                class,
                name: nm,
                proto: self.proto()?,
            }))
        } else {
            self.expect(":")?;
            Ok(Member::Field(FieldRef {
                class,
                name: nm,
                ty: self.ty()?,
            }))
        }
    }
    fn field_ref(&mut self) -> R<FieldRef> {
        match self.member()? {
            Member::Field(f) => Ok(f),
            _ => self.err("expected a field reference"),
        }
    }
    fn method_ref(&mut self) -> R<MethodRef> {
        match self.member()? {
            Member::Method(m) => Ok(m),
            _ => self.err("expected a method reference"),
        }
    }
    fn string(&mut self) -> R<S> {
        self.ws();
        let q = self.peek();
        if q != Some(b'"') && q != Some(b'\'') {
            return self.err("expected a string");
        }
        let q = q.unwrap();
        self.i += 1;
        let mut units: Vec<u16> = vec![];
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated string");
            };
            if c == q {
                self.i += 1;
                break;
            }
            if c == b'\n' {
                return self.err("unterminated string");
            }
            if c == b'\\' {
                self.i += 1;
                let e = self.peek().ok_or("unterminated string")?;
                self.i += 1;
                match e {
                    b'n' => units.push(10),
                    b'r' => units.push(13),
                    b't' => units.push(9),
                    b'b' => units.push(8),
                    b'f' => units.push(12),
                    b'0' => units.push(0),
                    b'u' => {
                        let h = self
                            .s
                            .get(self.i..self.i + 4)
                            .and_then(|h| std::str::from_utf8(h).ok())
                            .and_then(|h| u16::from_str_radix(h, 16).ok());
                        let Some(h) = h else {
                            return self.err("bad \\u escape");
                        };
                        units.push(h);
                        self.i += 4;
                    }
                    _ => units.push(e as u16),
                }
                continue;
            }
            // one UTF-8 character
            let len = match c {
                0..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            let ch = std::str::from_utf8(self.s.get(self.i..self.i + len).unwrap_or(&[]))
                .map_err(|e| e.to_string())?;
            let mut buf = [0u16; 2];
            units.extend_from_slice(ch.chars().next().unwrap().encode_utf16(&mut buf));
            self.i += len;
        }
        Ok(Arc::from(mutf8(&units)))
    }
    fn reg(&mut self, base: Option<u16>) -> R<u16> {
        self.ws();
        let w = {
            let st = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                self.i += 1;
            }
            std::str::from_utf8(&self.s[st..self.i]).unwrap()
        };
        let n = w.get(1..).and_then(|n| n.parse::<u16>().ok());
        match (w.as_bytes().first(), n) {
            (Some(b'v'), Some(n)) => Ok(n),
            (Some(b'p'), Some(n)) => match base {
                Some(b) => Ok(b + n),
                None => self.err("p-register used before .locals or .registers"),
            },
            _ => self.err(format!("expected a register, found `{w}`")),
        }
    }
    fn number(&mut self) -> R<i64> {
        let w = self.word();
        parse_int(w).ok_or_else(|| {
            self.err::<()>(format!("expected a number, found `{w}`"))
                .unwrap_err()
        })
    }
    fn label(&mut self, labels: &mut Labels) -> R<u32> {
        self.ws();
        if self.peek() != Some(b':') {
            return self.err("expected a label");
        }
        let w = self.word();
        Ok(labels.id(w, self.i))
    }

    fn value(&mut self, depth: u32) -> R<Value> {
        if depth > 64 {
            return self.err("value nesting too deep");
        }
        self.ws();
        match self.peek() {
            Some(b'"') => return Ok(Value::Str(self.string()?)),
            Some(b'\'') => {
                let s = self.string()?;
                let u = utf16(&s);
                if u.len() != 1 {
                    return self.err("a char literal holds one character");
                }
                return Ok(Value::Char(u[0]));
            }
            Some(b'{') => {
                self.i += 1;
                let mut v = vec![];
                loop {
                    self.wsn();
                    if self.eat("}") {
                        break;
                    }
                    v.push(self.value(depth + 1)?);
                    self.wsn();
                    if !self.eat(",") {
                        self.wsn();
                        self.expect("}")?;
                        break;
                    }
                }
                return Ok(Value::Array(v));
            }
            Some(b'(') => return Ok(Value::MethodType(self.proto()?)),
            _ => {}
        }
        if self.eat(".subannotation") {
            let ty = self.ty()?;
            let elems = self.elems(".end subannotation", depth)?;
            return Ok(Value::Anno(Anno { vis: 0, ty, elems }));
        }
        if self.eat(".enum") {
            return Ok(Value::Enum(self.field_ref()?));
        }
        // a type, or a member when `->` follows
        if matches!(self.peek(), Some(b'L' | b'[')) {
            let save = self.i;
            let t = self.ty()?;
            if self.at("->") {
                self.i = save;
                return Ok(match self.member()? {
                    Member::Field(f) => Value::Field(f),
                    Member::Method(m) => Value::Method(m),
                });
            }
            return Ok(Value::Type(t));
        }
        let save = self.i;
        let w = self.word();
        if let Some((k, _)) = w.split_once('@')
            && let Some(kind) = HANDLE_KINDS.iter().position(|x| *x == k)
        {
            self.i = save + k.len() + 1;
            return Ok(Value::MethodHandle(Box::new(Handle {
                kind: kind as u16,
                member: self.member()?,
            })));
        }
        Ok(match w {
            "null" => Value::Null,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "V" | "Z" | "B" | "S" | "C" | "I" | "J" | "F" | "D" => Value::Type(s(w.as_bytes())),
            _ => match literal(w) {
                Some(v) => v,
                None => return self.err(format!("bad value `{w}`")),
            },
        })
    }

    fn elems(&mut self, end: &str, depth: u32) -> R<Vec<(S, Value)>> {
        let mut out = vec![];
        loop {
            self.wsn();
            if self.eat(end) {
                return Ok(out);
            }
            if self.peek().is_none() {
                return self.err(format!("missing `{end}`"));
            }
            self.ws();
            let st = self.i;
            while self
                .peek()
                .is_some_and(|c| !c.is_ascii_whitespace() && c != b'=')
            {
                self.i += 1;
            }
            let n =
                text_bytes(std::str::from_utf8(&self.s[st..self.i]).map_err(|e| e.to_string())?);
            self.expect("=")?;
            out.push((n, self.value(depth + 1)?));
        }
    }

    fn annotation(&mut self) -> R<Anno> {
        // after `.annotation`
        let v = self.word();
        let vis = match VIS.iter().position(|x| *x == v) {
            Some(k) => k as u8,
            None => return self.err(format!("bad visibility `{v}` (build, runtime or system)")),
        };
        let ty = self.ty()?;
        let elems = self.elems(".end annotation", 0)?;
        Ok(Anno { vis, ty, elems })
    }

    /// hiddenapi words after the access flags: a list, then its domains
    fn hidden(&mut self) -> Option<u32> {
        let mut h = None;
        loop {
            let save = self.i;
            let w = self.word();
            if let Some(list) = HIDDEN_LISTS.iter().position(|&x| x == w) {
                h = Some(h.unwrap_or(0) | list as u32);
            } else if let Some(&(bit, _)) = HIDDEN_DOMAINS.iter().find(|x| x.1 == w) {
                h = Some(h.unwrap_or(0) | bit);
            } else if let Some(v) = w
                .strip_prefix(HIDDEN_RAW)
                .and_then(|v| v.strip_prefix("0x"))
                .and_then(|v| u32::from_str_radix(v, 16).ok())
            {
                h = Some(v);
            } else {
                self.i = save;
                return h;
            }
        }
    }

    /// flags words, then the rest of the line's first token (member spec)
    fn flags(&mut self, table: &[(u32, &str)]) -> R<u32> {
        let mut a = 0;
        loop {
            self.ws();
            let save = self.i;
            let w = self.word();
            if w.is_empty() {
                return self.err("missing name");
            }
            if let Some(&(bit, _)) = table.iter().find(|x| x.1 == w) {
                a |= bit;
            } else if let Some(v) = w
                .strip_prefix("0x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
            {
                a |= v;
            } else {
                self.i = save;
                return Ok(a);
            }
        }
    }
}

struct Labels {
    ids: HashMap<String, u32>,
    defined: HashMap<u32, ()>,
    first_use: HashMap<u32, usize>,
}
impl Labels {
    fn id(&mut self, name: &str, at: usize) -> u32 {
        let n = self.ids.len() as u32;
        let id = *self.ids.entry(name.to_string()).or_insert(n);
        self.first_use.entry(id).or_insert(at);
        id
    }
}

fn parse_int(w: &str) -> Option<i64> {
    let w = w.trim_end_matches(['L', 'l', 't', 'T', 's', 'S']);
    let (neg, w) = match w.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, w.strip_prefix('+').unwrap_or(w)),
    };
    let v = if let Some(h) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()?
    } else {
        w.parse::<u64>().ok()?
    };
    Some(if neg {
        (v as i64).wrapping_neg()
    } else {
        v as i64
    })
}

fn literal(w: &str) -> Option<Value> {
    let is_hex = w.trim_start_matches('-').starts_with("0x");
    if !is_hex {
        let fl = |x: &str| -> Option<f64> {
            match x.trim_start_matches('-') {
                "Infinity" => Some(if x.starts_with('-') {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }),
                "NaN" => Some(f64::NAN),
                _ => x.parse::<f64>().ok(),
            }
        };
        if let Some(x) = w.strip_suffix(['f', 'F'])
            && let Some(v) = fl(x)
        {
            // shortest-repr float text parses back to the same bits
            return Some(Value::Float(
                x.parse::<f32>()
                    .map(f32::to_bits)
                    .unwrap_or((v as f32).to_bits()),
            ));
        }
        if w.contains(['.', 'e', 'E']) || w.trim_start_matches('-') == "Infinity" || w == "NaN" {
            let x = w.strip_suffix(['d', 'D']).unwrap_or(w);
            return fl(x).map(|v| Value::Double(v.to_bits()));
        }
    }
    let v = parse_int(w)?;
    Some(match w.chars().last()? {
        'L' | 'l' => Value::Long(v),
        't' | 'T' => Value::Byte(v as i8),
        's' | 'S' => Value::Short(v as i16),
        _ => Value::Int(v as i32),
    })
}

/// Parse one class from Smali.
pub fn parse(text: &str) -> R<Class> {
    let mut p = P {
        s: text.as_bytes(),
        i: 0,
    };
    p.wsn();
    p.expect(".class")?;
    let access = p.flags(CLASS_FLAGS)?;
    let name = p.ty()?;
    p.eol()?;
    let mut c = Class {
        name,
        access,
        sup: None,
        ifaces: vec![],
        source: None,
        annos: vec![],
        fields: vec![],
        methods: vec![],
    };
    loop {
        p.wsn();
        if p.peek().is_none() {
            break;
        }
        if p.eat(".super") {
            c.sup = Some(p.ty()?);
        } else if p.eat(".source") {
            c.source = Some(p.string()?);
        } else if p.eat(".implements") {
            c.ifaces.push(p.ty()?);
        } else if p.eat(".annotation") {
            c.annos.push(p.annotation()?);
        } else if p.eat(".field") {
            c.fields.push(field(&mut p)?);
        } else if p.eat(".method") {
            c.methods.push(method_p(&mut p)?);
        } else {
            return p.err(format!("unexpected `{}`", p.rest_of_line()));
        }
        p.eol()?;
    }
    Ok(c)
}

fn spec_name(p: &mut P, stop: &[u8]) -> R<S> {
    p.ws();
    let st = p.i;
    while p
        .peek()
        .is_some_and(|c| !stop.contains(&c) && !c.is_ascii_whitespace())
    {
        p.i += 1;
    }
    if p.i == st {
        return p.err("missing name");
    }
    Ok(text_bytes(
        std::str::from_utf8(&p.s[st..p.i]).map_err(|e| e.to_string())?,
    ))
}

fn field(p: &mut P) -> R<Field> {
    let access = p.flags(FIELD_FLAGS)?;
    let hidden = p.hidden();
    let name = spec_name(p, b":")?;
    p.expect(":")?;
    let ty = p.ty()?;
    let init = if p.eat("=") { Some(p.value(0)?) } else { None };
    p.eol()?;
    let mut f = Field {
        name,
        ty,
        access,
        hidden,
        init,
        annos: vec![],
    };
    // optional annotations closed by .end field
    let save = p.i;
    p.wsn();
    if p.at(".annotation") || p.at(".end field") {
        loop {
            p.wsn();
            if p.eat(".end field") {
                break;
            }
            p.expect(".annotation")?;
            f.annos.push(p.annotation()?);
        }
    } else {
        p.i = save;
    }
    Ok(f)
}

fn method_p(p: &mut P) -> R<Method> {
    let access = p.flags(METHOD_FLAGS)?;
    let hidden = p.hidden();
    let name = spec_name(p, b"(")?;
    let proto = p.proto()?;
    p.eol()?;
    let mut m = Method {
        name,
        proto,
        access,
        hidden,
        code: None,
        annos: vec![],
        param_annos: None,
    };
    let ins = m.ins();
    let nparams = m.proto.params.len();
    // p-number -> parameter index
    let mut pidx = HashMap::new();
    let mut k = u16::from(access & ACC_STATIC == 0);
    for (i, t) in m.proto.params.iter().enumerate() {
        pidx.insert(k, i);
        k += width(t);
    }
    let mut registers: Option<u16> = None;
    let mut items: Vec<Item> = vec![];
    let mut tries: Vec<Try> = vec![];
    let mut names: Vec<Option<S>> = vec![None; nparams];
    let mut pannos: Vec<Vec<Anno>> = vec![vec![]; nparams];
    let mut labels = Labels {
        ids: HashMap::new(),
        defined: HashMap::new(),
        first_use: HashMap::new(),
    };
    let mut any_code = false;
    let base = |r: Option<u16>| r.map(|r| r - ins);
    loop {
        p.wsn();
        if p.peek().is_none() {
            return p.err("missing `.end method`");
        }
        if p.eat(".end method") {
            break;
        }
        if p.peek() == Some(b':') {
            let w = p.word();
            let id = labels.id(w, p.i);
            if labels.defined.insert(id, ()).is_some() {
                return p.err(format!("label {w} defined twice"));
            }
            items.push(Item::Label(id));
            p.eol()?;
            continue;
        }
        if p.peek() == Some(b'.') {
            let d = p.word();
            match d {
                ".locals" | ".registers" => {
                    let n = p.number()? as u16;
                    let r = if d == ".locals" { n + ins } else { n };
                    if r < ins {
                        return p.err(format!(
                            ".registers {r} is less than the {ins} the parameters need"
                        ));
                    }
                    registers = Some(r);
                }
                ".annotation" => m.annos.push(p.annotation()?),
                ".param" => {
                    p.ws();
                    let pn = match p.peek() {
                        Some(b'p') => p.reg(Some(0))?,
                        _ => {
                            let r = p.reg(None)?;
                            match base(registers) {
                                Some(b) if r >= b => r - b,
                                _ => return p.err("parameter register is not a parameter"),
                            }
                        }
                    };
                    let Some(&i) = pidx.get(&pn) else {
                        return p.err(format!("p{pn} is not a parameter"));
                    };
                    if p.eat(",") {
                        p.ws();
                        names[i] = if p.at("null") {
                            p.word();
                            None
                        } else {
                            Some(p.string()?)
                        };
                    }
                    p.eol()?;
                    let save = p.i;
                    p.wsn();
                    if p.eat(".end param") {
                        // bare `.param ... .end param`, no annotations
                    } else if p.at(".annotation") {
                        // annotations here belong to the param only if the block is closed by
                        // `.end param`; a bare param may instead be followed by a method-level
                        // annotation (e.g. Ldalvik/annotation/MethodParameters;), which baksmali
                        // also prints this way. Parse speculatively and rewind if not a param block.
                        let mut got = Vec::new();
                        let param_block = loop {
                            p.wsn();
                            if p.eat(".end param") {
                                break true;
                            }
                            if !p.at(".annotation") {
                                break false;
                            }
                            p.expect(".annotation")?;
                            got.push(p.annotation()?);
                        };
                        if param_block {
                            pannos[i].extend(got);
                        } else {
                            p.i = save; // method annotations: let the body loop read them
                        }
                    } else {
                        p.i = save;
                    }
                    continue;
                }
                ".line" => items.push(Item::Line(p.number()? as u32)),
                ".local" => {
                    let reg = p.reg(base(registers))?;
                    let (mut nm, mut ty, mut sig) = (None, None, None);
                    if p.eat(",") {
                        p.ws();
                        nm = if p.at("null") {
                            p.i += 4;
                            None
                        } else {
                            Some(p.string()?)
                        };
                        p.expect(":")?;
                        p.ws();
                        ty = if p.at("null") {
                            p.i += 4;
                            None
                        } else {
                            Some(p.ty()?)
                        };
                        if p.eat(",") {
                            sig = Some(p.string()?);
                        }
                    }
                    items.push(Item::Local {
                        reg,
                        name: nm,
                        ty,
                        sig,
                    });
                }
                ".end" => {
                    p.expect("local")?;
                    items.push(Item::EndLocal(p.reg(base(registers))?));
                }
                ".restart" => {
                    p.expect("local")?;
                    items.push(Item::Restart(p.reg(base(registers))?));
                }
                ".prologue" => items.push(Item::Prologue),
                ".epilogue" => items.push(Item::Epilogue),
                ".source" => {
                    p.ws();
                    items.push(Item::SetFile(if p.at("null") {
                        p.word();
                        None
                    } else {
                        Some(p.string()?)
                    }));
                }
                ".catch" | ".catchall" => {
                    let ty = if d == ".catch" { Some(p.ty()?) } else { None };
                    p.expect("{")?;
                    let a = p.label(&mut labels)?;
                    p.expect("..")?;
                    let b = p.label(&mut labels)?;
                    p.expect("}")?;
                    let h = p.label(&mut labels)?;
                    match tries.last_mut() {
                        Some(t) if t.start == a && t.end == b => t.handlers.push((ty, h)),
                        _ => tries.push(Try {
                            start: a,
                            end: b,
                            handlers: vec![(ty, h)],
                        }),
                    }
                }
                ".packed-switch" => {
                    let first = p.number()? as i32;
                    let mut targets = vec![];
                    loop {
                        p.wsn();
                        if p.eat(".end packed-switch") {
                            break;
                        }
                        targets.push(p.label(&mut labels)?);
                    }
                    items.push(Item::Packed { first, targets });
                }
                ".sparse-switch" => {
                    let mut v = vec![];
                    loop {
                        p.wsn();
                        if p.eat(".end sparse-switch") {
                            break;
                        }
                        let k = p.number()? as i32;
                        p.expect("->")?;
                        v.push((k, p.label(&mut labels)?));
                    }
                    items.push(Item::Sparse(v));
                }
                ".array-data" => {
                    let width = p.number()? as u16;
                    if !matches!(width, 1 | 2 | 4 | 8) {
                        return p.err("array element width must be 1, 2, 4 or 8");
                    }
                    let mut data = vec![];
                    loop {
                        p.wsn();
                        if p.eat(".end array-data") {
                            break;
                        }
                        let w = p.word();
                        let v = match literal(w) {
                            Some(Value::Float(b)) => b as i64,
                            Some(Value::Double(b)) => b as i64,
                            _ => match parse_int(w) {
                                Some(v) => v,
                                None => return p.err(format!("bad array element `{w}`")),
                            },
                        };
                        data.extend_from_slice(&v.to_le_bytes()[..width as usize]);
                    }
                    items.push(Item::Array { width, data });
                }
                _ => return p.err(format!("unknown directive `{d}`")),
            }
            p.eol()?;
            continue;
        }
        let mn = p.word();
        let Some(&op) = opcodes().get(mn) else {
            return p.err(format!("unknown instruction `{mn}`"));
        };
        any_code = true;
        let b = base(registers);
        let (_, f, kind) = bytecode::info(op).unwrap();
        let mut i = Ins {
            op,
            regs: [0; 5],
            n: 0,
            lit: 0,
            target: 0,
            r: Ref::None,
            proto: None,
        };
        let regs = |p: &mut P, k: usize, i: &mut Ins| -> R<()> {
            for (j, slot) in i.regs[..k].iter_mut().enumerate() {
                if j > 0 {
                    p.expect(",")?;
                }
                *slot = p.reg(b)?;
            }
            i.n = k as u8;
            Ok(())
        };
        let rf = |p: &mut P, i: &mut Ins| -> R<()> {
            p.expect(",")?;
            i.r = match kind {
                bytecode::Ref::String => Ref::Str(p.string()?),
                bytecode::Ref::Type => Ref::Type(p.ty()?),
                bytecode::Ref::Field => Ref::Field(p.field_ref()?),
                bytecode::Ref::Method => Ref::Method(p.method_ref()?),
                bytecode::Ref::Proto => Ref::Proto(p.proto()?),
                bytecode::Ref::MethodHandle => match p.value(0)? {
                    Value::MethodHandle(h) => Ref::Handle(*h),
                    _ => return p.err("expected a method handle like invoke-static@Lx;->m()V"),
                },
                bytecode::Ref::CallSite => {
                    p.ws();
                    while p.peek().is_some_and(|c| c != b'(') {
                        p.i += 1;
                    }
                    p.expect("(")?;
                    let mut v = vec![];
                    loop {
                        p.ws();
                        if p.eat(")") {
                            break;
                        }
                        v.push(p.value(0)?);
                        if !p.eat(",") {
                            p.expect(")")?;
                            break;
                        }
                    }
                    p.expect("@")?;
                    p.ws();
                    let h = if matches!(p.peek(), Some(b'L' | b'[')) {
                        Value::MethodHandle(Box::new(Handle {
                            kind: 4,
                            member: Member::Method(p.method_ref()?),
                        }))
                    } else {
                        p.value(0)?
                    };
                    v.insert(0, h);
                    Ref::CallSite(Arc::new(v))
                }
                bytecode::Ref::None => Ref::None,
            };
            Ok(())
        };
        let lit = |p: &mut P, i: &mut Ins| -> R<()> {
            p.expect(",")?;
            let w = p.word();
            i.lit = match literal(w) {
                Some(Value::Float(b)) => b as i32 as i64,
                Some(Value::Double(b)) => b as i64,
                _ => match parse_int(w) {
                    Some(v) => v,
                    None => return p.err(format!("bad literal `{w}`")),
                },
            };
            Ok(())
        };
        let tgt = |p: &mut P, i: &mut Ins, labels: &mut Labels, comma: bool| -> R<()> {
            if comma {
                p.expect(",")?;
            }
            i.target = p.label(labels)?;
            Ok(())
        };
        let list = |p: &mut P, i: &mut Ins, range: bool| -> R<()> {
            p.expect("{")?;
            if p.eat("}") {
                return Ok(());
            }
            if range {
                let a = p.reg(b)?;
                p.expect("..")?;
                let z = p.reg(b)?;
                if z < a {
                    return p.err("register range runs backwards");
                }
                i.regs[0] = a;
                i.n = (z - a + 1)
                    .try_into()
                    .map_err(|_| "register range too long".to_string())?;
            } else {
                let mut n = 0;
                loop {
                    if n == 5 {
                        return p.err("more than 5 registers (use the /range form)");
                    }
                    i.regs[n] = p.reg(b)?;
                    n += 1;
                    if !p.eat(",") {
                        break;
                    }
                }
                i.n = n as u8;
            }
            p.expect("}")
        };
        match f {
            Fmt::F10x => {}
            Fmt::F12x | Fmt::F22x | Fmt::F32x => regs(p, 2, &mut i)?,
            Fmt::F11n | Fmt::F21s | Fmt::F31i | Fmt::F21h | Fmt::F51l => {
                regs(p, 1, &mut i)?;
                lit(p, &mut i)?;
            }
            Fmt::F11x => regs(p, 1, &mut i)?,
            Fmt::F10t | Fmt::F20t | Fmt::F30t => tgt(p, &mut i, &mut labels, false)?,
            Fmt::F21t | Fmt::F31t => {
                regs(p, 1, &mut i)?;
                tgt(p, &mut i, &mut labels, true)?;
            }
            Fmt::F21c | Fmt::F31c => {
                regs(p, 1, &mut i)?;
                rf(p, &mut i)?;
            }
            Fmt::F23x => regs(p, 3, &mut i)?,
            Fmt::F22b | Fmt::F22s => {
                regs(p, 2, &mut i)?;
                lit(p, &mut i)?;
            }
            Fmt::F22t => {
                regs(p, 2, &mut i)?;
                tgt(p, &mut i, &mut labels, true)?;
            }
            Fmt::F22c => {
                regs(p, 2, &mut i)?;
                rf(p, &mut i)?;
            }
            Fmt::F35c | Fmt::F3rc => {
                list(p, &mut i, f == Fmt::F3rc)?;
                rf(p, &mut i)?;
            }
            Fmt::F45cc | Fmt::F4rcc => {
                list(p, &mut i, f == Fmt::F4rcc)?;
                rf(p, &mut i)?;
                p.expect(",")?;
                i.proto = Some(p.proto()?);
            }
        }
        items.push(Item::Ins(i));
        p.eol()?;
    }
    for (name, id) in &labels.ids {
        if !labels.defined.contains_key(id) {
            return Err(format!(
                "{}: label {name} is used but never defined",
                String::from_utf8_lossy(&m.name)
            ));
        }
    }
    if pannos.iter().any(|v| !v.is_empty()) {
        m.param_annos = Some(pannos);
    }
    if registers.is_some() || any_code {
        let debug = names.iter().any(|n| n.is_some())
            || items.iter().any(|it| {
                matches!(
                    it,
                    Item::Line(_)
                        | Item::Local { .. }
                        | Item::EndLocal(_)
                        | Item::Restart(_)
                        | Item::Prologue
                        | Item::Epilogue
                        | Item::SetFile(_)
                )
            });
        m.code = Some(Code {
            registers: registers.unwrap_or(ins),
            items,
            tries,
            debug,
            param_names: if debug { names } else { vec![None; nparams] },
        });
    } else if names.iter().any(|n| n.is_some()) {
        return Err(format!(
            "{}: .param names need a method body",
            String::from_utf8_lossy(&m.name)
        ));
    }
    Ok(m)
}
