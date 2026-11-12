//! Class model -> DEX bytes: fresh, sorted id tables; each data section written once, in the
//! order later sections need its offsets.

use super::model::*;
use super::read;
use crate::bytecode::{self, Fmt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type R<T> = Result<T, String>;
const NO_INDEX: u32 = u32::MAX;

fn uleb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn index_map<T: Clone + Eq + std::hash::Hash>(v: &[T]) -> HashMap<T, u32> {
    v.iter()
        .enumerate()
        .map(|(i, x)| (x.clone(), i as u32))
        .collect()
}

fn shorty(p: &Proto) -> S {
    let c = |t: &[u8]| {
        if matches!(t[0], b'L' | b'[') {
            b'L'
        } else {
            t[0]
        }
    };
    let v: Vec<u8> = std::iter::once(c(&p.ret))
        .chain(p.params.iter().map(|t| c(t)))
        .collect();
    s(&v)
}

/// DEX sorts strings by UTF-16 code units; ASCII compares the same as bytes.
fn cmp16(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.cmp(b)
    } else {
        utf16(a).cmp(&utf16(b))
    }
}

#[derive(Default)]
struct Collect {
    strings: HashSet<S>,
    types: HashSet<S>,
    protos: HashSet<Proto>,
    fields: HashSet<FieldRef>,
    methods: HashSet<MethodRef>,
    handles: Vec<Handle>,
    handle_set: HashSet<Handle>,
    sites: Vec<Arc<Vec<Value>>>,
    site_set: HashSet<Arc<Vec<Value>>>,
}

impl Collect {
    fn str(&mut self, x: &S) {
        if !self.strings.contains(x) {
            self.strings.insert(x.clone());
        }
    }
    fn ty(&mut self, t: &S) {
        if !self.types.contains(t) {
            self.types.insert(t.clone());
            self.str(t);
        }
    }
    fn proto(&mut self, p: &Proto) {
        if !self.protos.contains(p) {
            self.protos.insert(p.clone());
            self.str(&shorty(p));
            self.ty(&p.ret);
            p.params.iter().for_each(|t| self.ty(t));
        }
    }
    fn field(&mut self, f: &FieldRef) {
        if !self.fields.contains(f) {
            self.fields.insert(f.clone());
            self.ty(&f.class);
            self.str(&f.name);
            self.ty(&f.ty);
        }
    }
    fn method(&mut self, m: &MethodRef) {
        if !self.methods.contains(m) {
            self.methods.insert(m.clone());
            self.ty(&m.class);
            self.str(&m.name);
            self.proto(&m.proto);
        }
    }
    fn handle(&mut self, h: &Handle) {
        match &h.member {
            Member::Field(f) => self.field(f),
            Member::Method(m) => self.method(m),
        }
        if self.handle_set.insert(h.clone()) {
            self.handles.push(h.clone());
        }
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::MethodType(p) => self.proto(p),
            Value::MethodHandle(h) => self.handle(h),
            Value::Str(x) => self.str(x),
            Value::Type(t) => self.ty(t),
            Value::Field(f) | Value::Enum(f) => self.field(f),
            Value::Method(m) => self.method(m),
            Value::Array(a) => a.iter().for_each(|x| self.value(x)),
            Value::Anno(a) => self.anno(a),
            _ => {}
        }
    }
    fn anno(&mut self, a: &Anno) {
        self.ty(&a.ty);
        for (n, v) in &a.elems {
            self.str(n);
            self.value(v);
        }
    }
    fn class(&mut self, c: &Class) {
        self.ty(&c.name);
        c.sup.iter().for_each(|t| self.ty(t));
        c.ifaces.iter().for_each(|t| self.ty(t));
        c.source.iter().for_each(|x| self.str(x));
        c.annos.iter().for_each(|a| self.anno(a));
        for f in &c.fields {
            self.field(&FieldRef {
                class: c.name.clone(),
                name: f.name.clone(),
                ty: f.ty.clone(),
            });
            f.init.iter().for_each(|v| self.value(v));
            f.annos.iter().for_each(|a| self.anno(a));
        }
        for m in &c.methods {
            self.method(&MethodRef {
                class: c.name.clone(),
                name: m.name.clone(),
                proto: m.proto.clone(),
            });
            m.annos.iter().for_each(|a| self.anno(a));
            m.param_annos
                .iter()
                .flatten()
                .flatten()
                .for_each(|a| self.anno(a));
            let Some(code) = &m.code else { continue };
            code.param_names.iter().flatten().for_each(|x| self.str(x));
            for t in &code.tries {
                t.handlers
                    .iter()
                    .filter_map(|h| h.0.as_ref())
                    .for_each(|x| self.ty(x));
            }
            for it in &code.items {
                match it {
                    Item::Ins(i) => {
                        match &i.r {
                            Ref::None => {}
                            Ref::Str(x) => self.str(x),
                            Ref::Type(t) => self.ty(t),
                            Ref::Field(f) => self.field(f),
                            Ref::Method(m) => self.method(m),
                            Ref::Proto(p) => self.proto(p),
                            Ref::CallSite(cs) => {
                                cs.iter().for_each(|v| self.value(v));
                                if self.site_set.insert(cs.clone()) {
                                    self.sites.push(cs.clone());
                                }
                            }
                            Ref::Handle(h) => self.handle(h),
                        }
                        i.proto.iter().for_each(|p| self.proto(p));
                    }
                    Item::Local { name, ty, sig, .. } => [name, ty, sig]
                        .into_iter()
                        .flatten()
                        .for_each(|x| self.str(x)),
                    Item::SetFile(Some(x)) => self.str(x),
                    _ => {}
                }
            }
        }
    }
}

pub fn default_value(t: &[u8]) -> Value {
    match t {
        b"Z" => Value::Bool(false),
        b"B" => Value::Byte(0),
        b"S" => Value::Short(0),
        b"C" => Value::Char(0),
        b"I" => Value::Int(0),
        b"J" => Value::Long(0),
        b"F" => Value::Float(0),
        b"D" => Value::Double(0),
        _ => Value::Null,
    }
}

/// Final indices of everything a DEX refers to.
struct Pools {
    strings: Vec<S>,
    types: Vec<S>,
    protos: Vec<Proto>,
    fields: Vec<FieldRef>,
    methods: Vec<MethodRef>,
    handles: Vec<Handle>,
    sites: Vec<Arc<Vec<Value>>>,
    si: HashMap<S, u32>,
    ti: HashMap<S, u32>,
    pi: HashMap<Proto, u32>,
    fi: HashMap<FieldRef, u32>,
    mi: HashMap<MethodRef, u32>,
    hi: HashMap<Handle, u32>,
    ci: HashMap<Arc<Vec<Value>>, u32>,
}

impl Pools {
    fn new(c: Collect) -> R<Self> {
        let mut strings: Vec<S> = c.strings.into_iter().collect();
        strings.sort_unstable_by(|a, b| cmp16(a, b));
        let si: HashMap<S, u32> = strings
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        let mut types: Vec<S> = c.types.into_iter().collect();
        types.sort_unstable_by_key(|t| si[t]);
        let ti: HashMap<S, u32> = types
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        let mut protos: Vec<Proto> = c.protos.into_iter().collect();
        protos.sort_unstable_by_key(|p| {
            (
                ti[&p.ret],
                p.params.iter().map(|t| ti[t]).collect::<Vec<_>>(),
            )
        });
        let pi: HashMap<Proto, u32> = protos
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        let mut fields: Vec<FieldRef> = c.fields.into_iter().collect();
        fields.sort_unstable_by_key(|f| (ti[&f.class], si[&f.name], ti[&f.ty]));
        let fi: HashMap<FieldRef, u32> = fields
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        let mut methods: Vec<MethodRef> = c.methods.into_iter().collect();
        methods.sort_unstable_by_key(|m| (ti[&m.class], si[&m.name], pi[&m.proto]));
        let mi: HashMap<MethodRef, u32> = methods
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        for (what, n) in [
            ("types", types.len()),
            ("protos", protos.len()),
            ("fields", fields.len()),
            ("methods", methods.len()),
        ] {
            if n > 0x10000 {
                return Err(format!(
                    "too many {what} for one DEX file ({n}, the limit is 65536)"
                ));
            }
        }
        let hi = c
            .handles
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        let ci = c
            .sites
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32))
            .collect();
        Ok(Pools {
            strings,
            types,
            protos,
            fields,
            methods,
            handles: c.handles,
            sites: c.sites,
            si,
            ti,
            pi,
            fi,
            mi,
            hi,
            ci,
        })
    }

    /// Reuse a DEX's own id tables, in their existing (sorted) order, as the write pools. Lets an
    /// edit be assembled against the indices already in the file, so untouched classes stay valid.
    fn from_reader(r: &read::Reader<'_>) -> Self {
        let (strings, types, protos, fields, methods, handles, sites) = r.pool_tables();
        Pools {
            si: index_map(strings),
            ti: index_map(types),
            pi: index_map(protos),
            fi: index_map(fields),
            mi: index_map(methods),
            hi: index_map(handles),
            ci: index_map(sites),
            strings: strings.to_vec(),
            types: types.to_vec(),
            protos: protos.to_vec(),
            fields: fields.to_vec(),
            methods: methods.to_vec(),
            handles: handles.to_vec(),
            sites: sites.to_vec(),
        }
    }

    fn value(&self, v: &Value, out: &mut Vec<u8>) {
        // little-endian, fewest bytes: signed, unsigned, or right-zero-extended (floats)
        let signed = |out: &mut Vec<u8>, t: u8, x: i64| {
            let mut n = 1;
            while n < 8 && !((x << (64 - 8 * n)) >> (64 - 8 * n) == x) {
                n += 1;
            }
            out.push(((n as u8 - 1) << 5) | t);
            out.extend_from_slice(&x.to_le_bytes()[..n]);
        };
        let unsigned = |out: &mut Vec<u8>, t: u8, x: u64| {
            let n = (8 - x.leading_zeros() as usize / 8).max(1);
            out.push(((n as u8 - 1) << 5) | t);
            out.extend_from_slice(&x.to_le_bytes()[..n]);
        };
        let right = |out: &mut Vec<u8>, t: u8, x: u64, size: usize| {
            let n = (size - (x.trailing_zeros() as usize / 8).min(size)).max(1);
            out.push(((n as u8 - 1) << 5) | t);
            out.extend_from_slice(&x.to_le_bytes()[size - n..size]);
        };
        match v {
            Value::Byte(x) => out.extend([0x00, *x as u8]),
            Value::Short(x) => signed(out, 0x02, *x as i64),
            Value::Char(x) => unsigned(out, 0x03, *x as u64),
            Value::Int(x) => signed(out, 0x04, *x as i64),
            Value::Long(x) => signed(out, 0x06, *x),
            Value::Float(x) => right(out, 0x10, *x as u64, 4),
            Value::Double(x) => right(out, 0x11, *x, 8),
            Value::MethodType(p) => unsigned(out, 0x15, self.pi[p] as u64),
            Value::MethodHandle(h) => unsigned(out, 0x16, self.hi[&**h] as u64),
            Value::Str(x) => unsigned(out, 0x17, self.si[x] as u64),
            Value::Type(t) => unsigned(out, 0x18, self.ti[t] as u64),
            Value::Field(f) => unsigned(out, 0x19, self.fi[f] as u64),
            Value::Method(m) => unsigned(out, 0x1a, self.mi[m] as u64),
            Value::Enum(f) => unsigned(out, 0x1b, self.fi[f] as u64),
            Value::Array(a) => {
                out.push(0x1c);
                uleb(out, a.len() as u32);
                a.iter().for_each(|x| self.value(x, out));
            }
            Value::Anno(a) => {
                out.push(0x1d);
                self.anno(a, out);
            }
            Value::Null => out.push(0x1e),
            Value::Bool(b) => out.push((*b as u8) << 5 | 0x1f),
        }
    }

    fn anno(&self, a: &Anno, out: &mut Vec<u8>) {
        uleb(out, self.ti[&a.ty]);
        uleb(out, a.elems.len() as u32);
        let mut els: Vec<(u32, &Value)> = a.elems.iter().map(|(n, v)| (self.si[n], v)).collect();
        els.sort_by_key(|e| e.0);
        for (n, v) in els {
            uleb(out, n);
            self.value(v, out);
        }
    }

    fn opt(&self, x: &Option<S>) -> u32 {
        x.as_ref().map_or(NO_INDEX, |x| self.si[x])
    }
    fn p1(&self, x: &Option<S>) -> u32 {
        x.as_ref().map_or(0, |x| self.si[x] + 1)
    }
}

/// One method body laid out: code units, try table, debug events at their addresses.
struct Assembled {
    insns: Vec<u16>,
    outs: u16,
    /// (start, count, handler list index)
    tries: Vec<(u32, u16, usize)>,
    handlers: Vec<Vec<(Option<u32>, u32)>>,
    events: Vec<(u32, Item)>,
}

fn fits(v: i64, bits: u32) -> bool {
    let lim = 1i64 << (bits - 1);
    (-lim..lim).contains(&v)
}

fn assemble(code: &Code, p: &Pools, what: &dyn Fn() -> String) -> R<Assembled> {
    let err = |m: String| format!("{}: {m}", what());
    // choose final opcodes: const-string widens past 65535 strings, gotos widen to reach
    let mut ops: Vec<u8> = code
        .items
        .iter()
        .map(|it| if let Item::Ins(i) = it { i.op } else { 0 })
        .collect();
    for (k, it) in code.items.iter().enumerate() {
        if let Item::Ins(i) = it
            && i.op == 0x1a
            && let Ref::Str(x) = &i.r
            && p.si[x] > 0xffff
        {
            ops[k] = 0x1b;
        }
    }
    let mut offs = vec![0u32; code.items.len()];
    let mut labels: HashMap<u32, u32> = HashMap::new();
    loop {
        labels.clear();
        let mut off = 0u32;
        let mut pending: Vec<u32> = vec![];
        for (k, it) in code.items.iter().enumerate() {
            let size = match it {
                Item::Label(id) => {
                    labels.insert(*id, off);
                    pending.push(*id);
                    offs[k] = off;
                    continue;
                }
                Item::Ins(_) => bytecode::width(
                    bytecode::info(ops[k])
                        .ok_or_else(|| err(format!("unknown opcode {:#x}", ops[k])))?
                        .1,
                ) as u32,
                Item::Packed { targets, .. } => 4 + targets.len() as u32 * 2,
                Item::Sparse(v) => 2 + v.len() as u32 * 4,
                Item::Array { data, .. } => 4 + (data.len() as u32).div_ceil(2),
                _ => 0,
            };
            if matches!(
                it,
                Item::Packed { .. } | Item::Sparse(_) | Item::Array { .. }
            ) && off % 2 == 1
            {
                off += 1; // payloads are 4-byte aligned; labels just before move with them
                for id in &pending {
                    *labels.get_mut(id).unwrap() = off;
                }
            }
            if size > 0 {
                pending.clear();
            }
            offs[k] = off;
            off += size;
        }
        let mut changed = false;
        for (k, it) in code.items.iter().enumerate() {
            if let Item::Ins(i) = it
                && matches!(ops[k], 0x28 | 0x29)
            {
                let to = *labels
                    .get(&i.target)
                    .ok_or_else(|| err(format!("undefined label {}", i.target)))?;
                let rel = to as i64 - offs[k] as i64;
                let need = if rel != 0 && fits(rel, 8) {
                    0x28
                } else if rel != 0 && fits(rel, 16) {
                    0x29
                } else {
                    0x2a
                };
                if need > ops[k] {
                    ops[k] = need;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let label = |id: u32| {
        labels
            .get(&id)
            .copied()
            .ok_or_else(|| err(format!("undefined label {id}")))
    };

    // which switch uses each payload: payload targets are relative to it
    let mut users: HashMap<u32, u32> = HashMap::new();
    for (k, it) in code.items.iter().enumerate() {
        if let Item::Ins(i) = it
            && matches!(ops[k], 0x2b | 0x2c)
        {
            users.insert(label(i.target)?, offs[k]);
        }
    }

    let mut w: Vec<u16> = Vec::with_capacity(code.items.len() * 2);
    let mut events = vec![];
    let mut outs = 0u16;
    // debug events after the last label share its address, so they move past payload padding with it
    let mut mark = 0;
    for (k, it) in code.items.iter().enumerate() {
        let at = offs[k];
        if matches!(
            it,
            Item::Packed { .. } | Item::Sparse(_) | Item::Array { .. }
        ) && w.len() as u32 % 2 == 1
        {
            w.push(0);
            events[mark..]
                .iter_mut()
                .for_each(|e: &mut (u32, Item)| e.0 += 1);
        }
        match it {
            Item::Label(_) => mark = events.len(),
            Item::Ins(_) | Item::Packed { .. } | Item::Sparse(_) | Item::Array { .. } => {
                mark = events.len()
            }
            _ => {}
        }
        if matches!(
            it,
            Item::Ins(_) | Item::Packed { .. } | Item::Sparse(_) | Item::Array { .. }
        ) {
            debug_assert_eq!(w.len() as u32, at);
        }
        let lo = |v: u32| v as u16;
        let hi = |v: u32| (v >> 16) as u16;
        match it {
            Item::Label(_) => {}
            Item::Ins(i) => {
                let op = ops[k];
                let (name, f, _) = bytecode::info(op).unwrap();
                let bad = |m: &str| err(format!("{name}: {m}"));
                let r = |j: usize| i.regs[j];
                let reg4 = |j: usize| {
                    if r(j) < 16 {
                        Ok(r(j))
                    } else {
                        Err(bad(&format!(
                            "register v{} does not fit in 4 bits (use v0-v15)",
                            r(j)
                        )))
                    }
                };
                let reg8 = |j: usize| {
                    if r(j) < 256 {
                        Ok(r(j))
                    } else {
                        Err(bad(&format!(
                            "register v{} does not fit in 8 bits (use v0-v255)",
                            r(j)
                        )))
                    }
                };
                let lit_fit = |bits: u32| {
                    if fits(i.lit, bits) {
                        Ok(i.lit)
                    } else {
                        Err(bad(&format!(
                            "literal {:#x} does not fit in {bits} bits",
                            i.lit
                        )))
                    }
                };
                let rel = || -> R<i64> { Ok(label(i.target)? as i64 - at as i64) };
                let idx = || -> R<u32> {
                    Ok(match &i.r {
                        Ref::None => 0,
                        Ref::Str(x) => p.si[x],
                        Ref::Type(t) => p.ti[t],
                        Ref::Field(x) => p.fi[x],
                        Ref::Method(m) => p.mi[m],
                        Ref::Proto(x) => p.pi[x],
                        Ref::CallSite(c) => p.ci[c],
                        Ref::Handle(h) => p.hi[h],
                    })
                };
                let op = op as u16;
                match f {
                    Fmt::F10x => w.push(op),
                    Fmt::F12x => w.push(op | reg4(0)? << 8 | reg4(1)? << 12),
                    Fmt::F11n => w.push(op | reg4(0)? << 8 | ((lit_fit(4)? as u16) & 0xf) << 12),
                    Fmt::F11x => w.push(op | reg8(0)? << 8),
                    Fmt::F10t => w.push(op | ((rel()? as i8 as u8 as u16) << 8)),
                    Fmt::F20t => w.extend([op, rel()? as i16 as u16]),
                    Fmt::F22x => w.extend([op | reg8(0)? << 8, r(1)]),
                    Fmt::F21t => {
                        let v = rel()?;
                        if !fits(v, 16) {
                            return Err(bad("branch target too far (more than 32767 code units)"));
                        }
                        w.extend([op | reg8(0)? << 8, v as u16])
                    }
                    Fmt::F21s => w.extend([op | reg8(0)? << 8, lit_fit(16)? as u16]),
                    Fmt::F21h => {
                        let sh = if op == 0x19 { 48 } else { 16 };
                        let v = if sh == 16 { i.lit as i32 as i64 } else { i.lit };
                        if v != i.lit || v & ((1i64 << sh) - 1) != 0 {
                            return Err(bad(&format!(
                                "literal {:#x} must have only its top 16 bits set",
                                i.lit
                            )));
                        }
                        w.extend([op | reg8(0)? << 8, (i.lit >> sh) as u16])
                    }
                    Fmt::F21c => {
                        let x = idx()?;
                        if x > 0xffff {
                            return Err(bad("index does not fit in 16 bits"));
                        }
                        w.extend([op | reg8(0)? << 8, x as u16])
                    }
                    Fmt::F23x => w.extend([op | reg8(0)? << 8, reg8(1)? | reg8(2)? << 8]),
                    Fmt::F22b => w.extend([
                        op | reg8(0)? << 8,
                        reg8(1)? | ((lit_fit(8)? as u8 as u16) << 8),
                    ]),
                    Fmt::F22t => {
                        let v = rel()?;
                        if !fits(v, 16) {
                            return Err(bad("branch target too far (more than 32767 code units)"));
                        }
                        w.extend([op | reg4(0)? << 8 | reg4(1)? << 12, v as u16])
                    }
                    Fmt::F22s => {
                        w.extend([op | reg4(0)? << 8 | reg4(1)? << 12, lit_fit(16)? as u16])
                    }
                    Fmt::F22c => w.extend([op | reg4(0)? << 8 | reg4(1)? << 12, idx()? as u16]),
                    Fmt::F30t => {
                        let v = rel()? as i32 as u32;
                        w.extend([op, lo(v), hi(v)])
                    }
                    Fmt::F32x => w.extend([op, r(0), r(1)]),
                    Fmt::F31i => {
                        let v = lit_fit(32)? as i32 as u32;
                        w.extend([op | reg8(0)? << 8, lo(v), hi(v)])
                    }
                    Fmt::F31t => {
                        let v = rel()? as i32 as u32;
                        w.extend([op | reg8(0)? << 8, lo(v), hi(v)])
                    }
                    Fmt::F31c => {
                        let v = idx()?;
                        w.extend([op | reg8(0)? << 8, lo(v), hi(v)])
                    }
                    Fmt::F35c | Fmt::F45cc => {
                        let n = i.n as usize;
                        if n > 5 {
                            return Err(bad("more than 5 registers (use the /range form)"));
                        }
                        let mut g = [0u16; 5];
                        for (j, slot) in g.iter_mut().enumerate().take(n) {
                            *slot = reg4(j)?;
                        }
                        let x = idx()?;
                        if x > 0xffff {
                            return Err(bad("index does not fit in 16 bits"));
                        }
                        w.extend([
                            op | (n as u16) << 12 | g[4] << 8,
                            x as u16,
                            g[0] | g[1] << 4 | g[2] << 8 | g[3] << 12,
                        ]);
                        if f == Fmt::F45cc {
                            w.push(
                                p.pi[i.proto.as_ref().ok_or_else(|| bad("missing proto"))?] as u16,
                            );
                        }
                    }
                    Fmt::F3rc | Fmt::F4rcc => {
                        let x = idx()?;
                        w.extend([op | (i.n as u16) << 8, x as u16, r(0)]);
                        if f == Fmt::F4rcc {
                            w.push(
                                p.pi[i.proto.as_ref().ok_or_else(|| bad("missing proto"))?] as u16,
                            );
                        }
                    }
                    Fmt::F51l => {
                        let v = i.lit as u64;
                        w.extend([
                            op | reg8(0)? << 8,
                            v as u16,
                            (v >> 16) as u16,
                            (v >> 32) as u16,
                            (v >> 48) as u16,
                        ])
                    }
                }
                if matches!(op, 0x6e..=0x72 | 0x74..=0x78 | 0xfa..=0xfd) {
                    outs = outs.max(i.n as u16);
                }
            }
            Item::Packed { first, targets } => {
                let user = *users.get(&at).ok_or_else(|| {
                    err("packed-switch data with no packed-switch using it".into())
                })?;
                w.extend([
                    0x0100,
                    targets.len() as u16,
                    *first as u32 as u16,
                    (*first as u32 >> 16) as u16,
                ]);
                for t in targets {
                    let v = (label(*t)? as i64 - user as i64) as i32 as u32;
                    w.extend([lo(v), hi(v)]);
                }
            }
            Item::Sparse(v) => {
                let user = *users.get(&at).ok_or_else(|| {
                    err("sparse-switch data with no sparse-switch using it".into())
                })?;
                w.extend([0x0200, v.len() as u16]);
                for (key, _) in v {
                    w.extend([*key as u32 as u16, (*key as u32 >> 16) as u16]);
                }
                for (_, t) in v {
                    let x = (label(*t)? as i64 - user as i64) as i32 as u32;
                    w.extend([lo(x), hi(x)]);
                }
            }
            Item::Array { width, data } => {
                let n = if *width == 0 {
                    0
                } else {
                    data.len() as u32 / *width as u32
                };
                w.extend([0x0300, *width, lo(n), hi(n)]);
                let mut b = data.clone();
                if b.len() % 2 == 1 {
                    b.push(0);
                }
                w.extend(
                    b.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
            }
            ev => events.push((w.len() as u32, ev.clone())),
        }
    }

    let mut handlers: Vec<Vec<(Option<u32>, u32)>> = vec![];
    let mut tries = vec![];
    let mut sorted: Vec<&Try> = code.tries.iter().collect();
    sorted.sort_by_key(|t| labels.get(&t.start).copied().unwrap_or(0));
    let mut last_end = 0;
    for t in sorted {
        let (a, b) = (label(t.start)?, label(t.end)?);
        if b <= a || b - a > 0xffff {
            return Err(err(format!("bad try range {a:#x}..{b:#x}")));
        }
        if a < last_end {
            return Err(err(
                "try ranges overlap; split them so each address has one set of handlers".into(),
            ));
        }
        last_end = b;
        let h: Vec<(Option<u32>, u32)> = t
            .handlers
            .iter()
            .map(|(ty, l)| Ok((ty.as_ref().map(|x| p.ti[x]), label(*l)?)))
            .collect::<R<_>>()?;
        let k = handlers.iter().position(|x| *x == h).unwrap_or_else(|| {
            handlers.push(h);
            handlers.len() - 1
        });
        tries.push((a, (b - a) as u16, k));
    }
    Ok(Assembled {
        insns: w,
        outs,
        tries,
        handlers,
        events,
    })
}

struct Out {
    buf: Vec<u8>,
    base: u32,
    /// (map type, count, offset)
    map: Vec<(u16, u32, u32)>,
}

impl Out {
    fn pos(&self) -> u32 {
        self.base + self.buf.len() as u32
    }
    fn align(&mut self) {
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
    }
    fn section(&mut self, ty: u16, count: u32, off: u32) {
        if count > 0 {
            self.map.push((ty, count, off));
        }
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend(v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend(v.to_le_bytes());
    }
}

/// Dedups byte blobs within one section, writing each new one at the current position.
fn intern(
    out: &mut Out,
    seen: &mut HashMap<Vec<u8>, u32>,
    bytes: Vec<u8>,
    align: bool,
    count: &mut u32,
) -> u32 {
    if let Some(&o) = seen.get(&bytes) {
        return o;
    }
    if align {
        out.align();
    }
    let o = out.pos();
    out.buf.extend_from_slice(&bytes);
    seen.insert(bytes, o);
    *count += 1;
    o
}

fn type_list(p: &Pools, l: &[S]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + l.len() * 2);
    b.extend((l.len() as u32).to_le_bytes());
    for t in l {
        b.extend((p.ti[t] as u16).to_le_bytes());
    }
    b
}

/// `version` is the 3-digit DEX version to write (raised to 038 when the code needs it).
pub fn write(classes: &[Class], version: [u8; 3]) -> R<Vec<u8>> {
    let mut col = Collect::default();
    classes.iter().for_each(|c| col.class(c));
    let p = Pools::new(col)?;

    // superclasses and interfaces defined here must come first
    let by_name: HashMap<&[u8], usize> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| (&*c.name, i))
        .collect();
    if by_name.len() != classes.len() {
        return Err("a class is defined twice".into());
    }
    let mut order = Vec::with_capacity(classes.len());
    let mut state = vec![0u8; classes.len()];
    fn visit(
        i: usize,
        cs: &[Class],
        by: &HashMap<&[u8], usize>,
        st: &mut [u8],
        order: &mut Vec<usize>,
    ) -> R<()> {
        match st[i] {
            2 => return Ok(()),
            1 => {
                return Err(format!(
                    "class {} inherits from itself",
                    String::from_utf8_lossy(&cs[i].name)
                ));
            }
            _ => {}
        }
        st[i] = 1;
        for t in cs[i].sup.iter().chain(&cs[i].ifaces) {
            if let Some(&j) = by.get(&**t) {
                visit(j, cs, by, st, order)?;
            }
        }
        st[i] = 2;
        order.push(i);
        Ok(())
    }
    for i in 0..classes.len() {
        visit(i, classes, &by_name, &mut state, &mut order)?;
    }
    // everything below is laid out in class_def order, so a re-read file writes back identically
    let classes: &[&Class] = &order.iter().map(|&i| &classes[i]).collect::<Vec<_>>();

    let needs38 = !p.sites.is_empty() || !p.handles.is_empty();
    let version = if needs38 && version < *b"038" {
        *b"038"
    } else {
        version
    };

    let (ns, nt, np, nf, nm, nc, ncs, nmh) = (
        p.strings.len(),
        p.types.len(),
        p.protos.len(),
        p.fields.len(),
        p.methods.len(),
        classes.len(),
        p.sites.len(),
        p.handles.len(),
    );
    let ids = [
        (1u16, ns, 4usize),
        (2, nt, 4),
        (3, np, 12),
        (4, nf, 8),
        (5, nm, 8),
        (6, nc, 32),
        (7, ncs, 4),
        (8, nmh, 8),
    ];
    let mut id_off = [0u32; 9];
    let mut at = 0x70u32;
    for (ty, n, size) in ids {
        id_off[ty as usize] = if n == 0 { 0 } else { at };
        at += (n * size) as u32;
    }
    let mut out = Out {
        buf: Vec::with_capacity(1 << 20),
        base: at,
        map: vec![],
    };

    // string data
    let start = out.pos();
    let mut string_off = Vec::with_capacity(ns);
    for x in &p.strings {
        string_off.push(out.pos());
        let units = x.iter().filter(|&&b| b & 0xc0 != 0x80).count() as u32;
        uleb(&mut out.buf, units);
        out.buf.extend_from_slice(x);
        out.buf.push(0);
    }
    out.section(0x2002, ns as u32, start);

    // type lists: proto parameters and interfaces
    out.align();
    let (start, mut count, mut seen) = (out.pos(), 0, HashMap::new());
    let proto_params: Vec<u32> = p
        .protos
        .iter()
        .map(|x| {
            if x.params.is_empty() {
                0
            } else {
                intern(
                    &mut out,
                    &mut seen,
                    type_list(&p, &x.params),
                    true,
                    &mut count,
                )
            }
        })
        .collect();
    let ifaces: Vec<u32> = classes
        .iter()
        .map(|c| {
            if c.ifaces.is_empty() {
                0
            } else {
                intern(
                    &mut out,
                    &mut seen,
                    type_list(&p, &c.ifaces),
                    true,
                    &mut count,
                )
            }
        })
        .collect();
    out.section(0x1001, count, start);

    // annotations: items, then sets, then set-ref lists, then directories
    let (start, mut count, mut seen) = (out.pos(), 0, HashMap::new());
    let mut set_specs: Vec<Vec<(u32, u32)>> = vec![];
    let mut spec =
        |out: &mut Out, seen: &mut HashMap<Vec<u8>, u32>, count: &mut u32, set: &[Anno]| -> usize {
            let mut v: Vec<(u32, u32)> = set
                .iter()
                .map(|a| {
                    let mut b = vec![a.vis];
                    p.anno(a, &mut b);
                    (p.ti[&a.ty], intern(out, seen, b, false, count))
                })
                .collect();
            v.sort_by_key(|x| x.0);
            set_specs.push(v);
            set_specs.len() - 1
        };
    struct Dir {
        class: Option<usize>,
        fields: Vec<(u32, usize)>,
        methods: Vec<(u32, usize)>,
        params: Vec<(u32, Vec<Option<usize>>)>,
    }
    let mut dirs: Vec<Option<Dir>> = Vec::with_capacity(nc);
    for c in classes {
        let mut d = Dir {
            class: None,
            fields: vec![],
            methods: vec![],
            params: vec![],
        };
        if !c.annos.is_empty() {
            d.class = Some(spec(&mut out, &mut seen, &mut count, &c.annos));
        }
        for f in &c.fields {
            if !f.annos.is_empty() {
                let idx = p.fi[&FieldRef {
                    class: c.name.clone(),
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                }];
                d.fields
                    .push((idx, spec(&mut out, &mut seen, &mut count, &f.annos)));
            }
        }
        for m in &c.methods {
            let idx = p.mi[&MethodRef {
                class: c.name.clone(),
                name: m.name.clone(),
                proto: m.proto.clone(),
            }];
            if !m.annos.is_empty() {
                d.methods
                    .push((idx, spec(&mut out, &mut seen, &mut count, &m.annos)));
            }
            if let Some(pa) = &m.param_annos {
                let sets = pa
                    .iter()
                    .map(|s| {
                        if s.is_empty() {
                            None
                        } else {
                            Some(spec(&mut out, &mut seen, &mut count, s))
                        }
                    })
                    .collect();
                d.params.push((idx, sets));
            }
        }
        d.fields.sort_by_key(|x| x.0);
        d.methods.sort_by_key(|x| x.0);
        d.params.sort_by_key(|x| x.0);
        let empty =
            d.class.is_none() && d.fields.is_empty() && d.methods.is_empty() && d.params.is_empty();
        dirs.push(if empty { None } else { Some(d) });
    }
    out.section(0x2004, count, start);
    out.align();
    let (start, mut count, mut seen) = (out.pos(), 0, HashMap::new());
    let set_off: Vec<u32> = set_specs
        .iter()
        .map(|v| {
            let mut b = Vec::with_capacity(4 + v.len() * 4);
            b.extend((v.len() as u32).to_le_bytes());
            v.iter().for_each(|x| b.extend(x.1.to_le_bytes()));
            intern(&mut out, &mut seen, b, true, &mut count)
        })
        .collect();
    out.section(0x1003, count, start);
    out.align();
    let (start, mut count, mut seen) = (out.pos(), 0, HashMap::new());
    let ref_lists: Vec<Vec<u32>> = dirs
        .iter()
        .map(|d| {
            d.iter()
                .flat_map(|d| &d.params)
                .map(|(_, sets)| {
                    let mut b = Vec::with_capacity(4 + sets.len() * 4);
                    b.extend((sets.len() as u32).to_le_bytes());
                    sets.iter()
                        .for_each(|s| b.extend(s.map_or(0, |k| set_off[k]).to_le_bytes()));
                    intern(&mut out, &mut seen, b, true, &mut count)
                })
                .collect()
        })
        .collect();
    out.section(0x1002, count, start);
    out.align();
    let (start, mut count) = (out.pos(), 0);
    let dir_off: Vec<u32> = dirs
        .iter()
        .zip(&ref_lists)
        .map(|(d, refs)| {
            let Some(d) = d else { return 0 };
            out.align();
            let o = out.pos();
            out.u32(d.class.map_or(0, |k| set_off[k]));
            out.u32(d.fields.len() as u32);
            out.u32(d.methods.len() as u32);
            out.u32(d.params.len() as u32);
            for (i, k) in d.fields.iter().chain(&d.methods) {
                out.u32(*i);
                out.u32(set_off[*k]);
            }
            for ((i, _), r) in d.params.iter().zip(refs) {
                out.u32(*i);
                out.u32(*r);
            }
            count += 1;
            o
        })
        .collect();
    out.section(0x2006, count, start);

    // encoded arrays: call sites, then static values
    let (start, mut count, mut seen) = (out.pos(), 0, HashMap::new());
    let site_off: Vec<u32> = p
        .sites
        .iter()
        .map(|cs| {
            let mut b = vec![];
            uleb(&mut b, cs.len() as u32);
            cs.iter().for_each(|v| p.value(v, &mut b));
            // call sites must not share an item: their ids are sorted by offset
            let o = out.pos();
            out.buf.extend_from_slice(&b);
            count += 1;
            o
        })
        .collect();
    let static_off: Vec<u32> = classes
        .iter()
        .map(|c| {
            let mut st: Vec<(u32, &Field)> = c
                .fields
                .iter()
                .filter(|f| f.access & ACC_STATIC != 0)
                .map(|f| {
                    (
                        p.fi[&FieldRef {
                            class: c.name.clone(),
                            name: f.name.clone(),
                            ty: f.ty.clone(),
                        }],
                        f,
                    )
                })
                .collect();
            st.sort_by_key(|x| x.0);
            let Some(last) = st.iter().rposition(|x| x.1.init.is_some()) else {
                return 0;
            };
            let mut b = vec![];
            uleb(&mut b, last as u32 + 1);
            for (_, f) in &st[..=last] {
                p.value(f.init.as_ref().unwrap_or(&default_value(&f.ty)), &mut b);
            }
            intern(&mut out, &mut seen, b, false, &mut count)
        })
        .collect();
    out.section(0x2005, count, start);

    // lay out every method body, then debug info, then code items
    let mut bodies: Vec<Vec<Option<(Assembled, u16)>>> = Vec::with_capacity(nc);
    for c in classes {
        let mut v = Vec::with_capacity(c.methods.len());
        for m in &c.methods {
            v.push(match &m.code {
                None => None,
                Some(code) => {
                    let what = || {
                        format!(
                            "{}->{}",
                            String::from_utf8_lossy(&c.name),
                            String::from_utf8_lossy(&m.name)
                        )
                    };
                    if m.ins() > code.registers {
                        return Err(format!(
                            "{}: .registers {} is less than the {} the parameters need",
                            what(),
                            code.registers,
                            m.ins()
                        ));
                    }
                    Some((assemble(code, &p, &what)?, m.ins()))
                }
            });
        }
        bodies.push(v);
    }
    let (start, mut count) = (out.pos(), 0);
    let mut debug_off: Vec<Vec<u32>> = Vec::with_capacity(nc);
    for (c, body) in classes.iter().zip(&bodies) {
        let mut v = Vec::with_capacity(c.methods.len());
        for (m, b) in c.methods.iter().zip(body) {
            let (Some(code), Some((a, _))) = (&m.code, b) else {
                v.push(0);
                continue;
            };
            if !code.debug {
                v.push(0);
                continue;
            }
            v.push(out.pos());
            count += 1;
            let first_line = a
                .events
                .iter()
                .find_map(|(_, e)| {
                    if let Item::Line(l) = e {
                        Some(*l)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let o = &mut out.buf;
            uleb(o, first_line);
            uleb(o, code.param_names.len() as u32);
            code.param_names.iter().for_each(|n| uleb(o, p.p1(n)));
            let (mut addr, mut line) = (0u32, first_line);
            for (at, ev) in &a.events {
                if let Item::Line(l) = ev {
                    let mut dl = *l as i64 - line as i64;
                    let mut da = *at - addr;
                    if !(-4..=10).contains(&dl) {
                        o.push(2);
                        sleb(o, dl as i32);
                        dl = 0;
                    }
                    if (dl + 4) as u32 + 15 * da + 0x0a > 0xff {
                        o.push(1);
                        uleb(o, da);
                        da = 0;
                    }
                    o.push(((dl + 4) as u32 + 15 * da + 0x0a) as u8);
                    (addr, line) = (*at, *l);
                    continue;
                }
                if *at > addr {
                    o.push(1);
                    uleb(o, at - addr);
                    addr = *at;
                }
                match ev {
                    Item::Local { reg, name, ty, sig } => {
                        o.push(if sig.is_some() { 4 } else { 3 });
                        uleb(o, *reg as u32);
                        uleb(o, p.p1(name));
                        uleb(o, p.p1(ty));
                        if sig.is_some() {
                            uleb(o, p.p1(sig));
                        }
                    }
                    Item::EndLocal(r) => {
                        o.push(5);
                        uleb(o, *r as u32);
                    }
                    Item::Restart(r) => {
                        o.push(6);
                        uleb(o, *r as u32);
                    }
                    Item::Prologue => o.push(7),
                    Item::Epilogue => o.push(8),
                    Item::SetFile(f) => {
                        o.push(9);
                        uleb(o, p.p1(f));
                    }
                    _ => {}
                }
            }
            o.push(0);
        }
        debug_off.push(v);
    }
    out.section(0x2003, count, start);

    out.align();
    let (start, mut count) = (out.pos(), 0);
    let mut code_off: Vec<Vec<u32>> = Vec::with_capacity(nc);
    for ((c, body), dbg) in classes.iter().zip(&bodies).zip(&debug_off) {
        let mut v = Vec::with_capacity(c.methods.len());
        for ((m, b), &dbg) in c.methods.iter().zip(body).zip(dbg) {
            let Some((a, ins)) = b else {
                v.push(0);
                continue;
            };
            out.align();
            v.push(out.pos());
            count += 1;
            out.u16(m.code.as_ref().unwrap().registers);
            out.u16(*ins);
            out.u16(a.outs);
            out.u16(a.tries.len() as u16);
            out.u32(dbg);
            out.u32(a.insns.len() as u32);
            a.insns
                .iter()
                .for_each(|&u| out.buf.extend(u.to_le_bytes()));
            if a.tries.is_empty() {
                continue;
            }
            if a.insns.len() % 2 == 1 {
                out.u16(0);
            }
            let mut list = vec![];
            uleb(&mut list, a.handlers.len() as u32);
            let mut hoff = vec![];
            for h in &a.handlers {
                hoff.push(list.len() as u16);
                let typed: Vec<_> = h.iter().filter(|x| x.0.is_some()).collect();
                let all = h.iter().find(|x| x.0.is_none());
                sleb(
                    &mut list,
                    if all.is_some() {
                        -(typed.len() as i32)
                    } else {
                        typed.len() as i32
                    },
                );
                for (t, addr) in typed {
                    uleb(&mut list, t.unwrap());
                    uleb(&mut list, *addr);
                }
                if let Some((_, addr)) = all {
                    uleb(&mut list, *addr);
                }
            }
            for (s, n, k) in &a.tries {
                out.u32(*s);
                out.u16(*n);
                out.u16(hoff[*k]);
            }
            out.buf.extend_from_slice(&list);
        }
        code_off.push(v);
    }
    out.section(0x2001, count, start);

    let (start, mut count) = (out.pos(), 0);
    let mut data_off = Vec::with_capacity(nc);
    let mut hidden: Vec<Vec<Option<u32>>> = Vec::with_capacity(nc);
    for (c, codes) in classes.iter().zip(&code_off) {
        if c.fields.is_empty() && c.methods.is_empty() {
            data_off.push(0);
            hidden.push(vec![]);
            continue;
        }
        let fid = |f: &Field| {
            p.fi[&FieldRef {
                class: c.name.clone(),
                name: f.name.clone(),
                ty: f.ty.clone(),
            }]
        };
        let mid = |m: &Method| {
            p.mi[&MethodRef {
                class: c.name.clone(),
                name: m.name.clone(),
                proto: m.proto.clone(),
            }]
        };
        let mut sf: Vec<(u32, u32, Option<u32>)> = c
            .fields
            .iter()
            .filter(|f| f.access & ACC_STATIC != 0)
            .map(|f| (fid(f), f.access, f.hidden))
            .collect();
        let mut inf: Vec<(u32, u32, Option<u32>)> = c
            .fields
            .iter()
            .filter(|f| f.access & ACC_STATIC == 0)
            .map(|f| (fid(f), f.access, f.hidden))
            .collect();
        let mut dm: Vec<(u32, u32, u32, Option<u32>)> = vec![];
        let mut vm: Vec<(u32, u32, u32, Option<u32>)> = vec![];
        for (m, &co) in c.methods.iter().zip(codes) {
            (if m.is_direct() { &mut dm } else { &mut vm }).push((mid(m), m.access, co, m.hidden));
        }
        sf.sort_unstable();
        inf.sort_unstable();
        dm.sort_unstable();
        vm.sort_unstable();
        data_off.push(out.pos());
        count += 1;
        // hiddenapi flags follow the members' order in class_data
        let flags = (sf.iter().chain(&inf).map(|x| x.2)).chain(dm.iter().chain(&vm).map(|x| x.3));
        hidden.push(flags.collect());
        let o = &mut out.buf;
        for n in [sf.len(), inf.len(), dm.len(), vm.len()] {
            uleb(o, n as u32);
        }
        for list in [&sf, &inf] {
            let mut prev = 0;
            for &(i, a, _) in list.iter() {
                uleb(o, i - prev);
                uleb(o, a);
                prev = i;
            }
        }
        for list in [&dm, &vm] {
            let mut prev = 0;
            for &(i, a, co, _) in list.iter() {
                if i == prev && prev != 0 {
                    return Err(format!(
                        "{}: method defined twice",
                        String::from_utf8_lossy(&c.name)
                    ));
                }
                uleb(o, i - prev);
                uleb(o, a);
                uleb(o, co);
                prev = i;
            }
        }
    }
    out.section(0x2000, count, start);

    // hiddenapi class data: the section's size, an offset per class_def (0 for a class without
    // flags), then the flags
    if hidden.iter().flatten().any(|h| h.is_some()) {
        out.align();
        let start = out.pos();
        let mut flags = vec![];
        let mut offsets = Vec::with_capacity(nc);
        for members in &hidden {
            if members.iter().all(|h| h.is_none()) {
                offsets.push(0);
                continue;
            }
            offsets.push((4 + nc * 4 + flags.len()) as u32);
            for h in members {
                uleb(&mut flags, h.unwrap_or(0));
            }
        }
        out.u32((4 + nc * 4 + flags.len()) as u32);
        offsets.into_iter().for_each(|o| out.u32(o));
        out.buf.extend_from_slice(&flags);
        out.section(0xf000, 1, start);
    }

    // map list
    out.align();
    let map_off = out.pos();
    let mut map: Vec<(u16, u32, u32)> = vec![(0, 1, 0)];
    for (ty, n, _) in ids {
        if n > 0 {
            map.push((ty, n as u32, id_off[ty as usize]));
        }
    }
    map.extend(out.map.iter().copied());
    map.push((0x1000, 1, map_off));
    out.u32(map.len() as u32);
    for (ty, n, o) in &map {
        out.u16(*ty);
        out.u16(0);
        out.u32(*n);
        out.u32(*o);
    }
    let data = out.buf;
    let data_start = at;

    // header + id tables
    let mut f: Vec<u8> = Vec::with_capacity(data_start as usize + data.len());
    let put = |f: &mut Vec<u8>, v: u32| f.extend(v.to_le_bytes());
    f.extend_from_slice(b"dex\n");
    f.extend_from_slice(&version);
    f.push(0);
    f.extend([0u8; 24]); // checksum + signature, filled last
    let file_size = data_start + data.len() as u32;
    put(&mut f, file_size);
    put(&mut f, 0x70);
    put(&mut f, 0x1234_5678);
    put(&mut f, 0);
    put(&mut f, 0);
    put(&mut f, map_off);
    for (ty, n, _) in &ids[..6] {
        put(&mut f, *n as u32);
        put(&mut f, id_off[*ty as usize]);
    }
    put(&mut f, data.len() as u32);
    put(&mut f, data_start);
    debug_assert_eq!(f.len(), 0x70);
    for o in &string_off {
        put(&mut f, *o);
    }
    for t in &p.types {
        put(&mut f, p.si[t]);
    }
    for (x, &params) in p.protos.iter().zip(&proto_params) {
        put(&mut f, p.si[&shorty(x)]);
        put(&mut f, p.ti[&x.ret]);
        put(&mut f, params);
    }
    for x in &p.fields {
        f.extend((p.ti[&x.class] as u16).to_le_bytes());
        f.extend((p.ti[&x.ty] as u16).to_le_bytes());
        put(&mut f, p.si[&x.name]);
    }
    for x in &p.methods {
        f.extend((p.ti[&x.class] as u16).to_le_bytes());
        f.extend((p.pi[&x.proto] as u16).to_le_bytes());
        put(&mut f, p.si[&x.name]);
    }
    for (i, c) in classes.iter().enumerate() {
        put(&mut f, p.ti[&c.name]);
        put(&mut f, c.access);
        put(&mut f, c.sup.as_ref().map_or(NO_INDEX, |t| p.ti[t]));
        put(&mut f, ifaces[i]);
        put(&mut f, p.opt(&c.source));
        put(&mut f, dir_off[i]);
        put(&mut f, data_off[i]);
        put(&mut f, static_off[i]);
    }
    for o in &site_off {
        put(&mut f, *o);
    }
    for h in &p.handles {
        let id = match &h.member {
            Member::Field(x) => p.fi[x],
            Member::Method(m) => p.mi[m],
        };
        f.extend(h.kind.to_le_bytes());
        f.extend([0, 0]);
        f.extend((id as u16).to_le_bytes());
        f.extend([0, 0]);
    }
    debug_assert_eq!(f.len() as u32, data_start);
    f.extend_from_slice(&data);
    seal(&mut f);
    Ok(f)
}

/// Patch edited classes into `dex` in place, without rebuilding the whole file. Only works when
/// every edit just changes instruction bytes of existing methods (same code layout, no new pool
/// entries, no field/annotation/signature changes); returns `None` to fall back to [`write`]
/// otherwise. Keeps peak memory near the DEX size instead of modelling every class.
pub fn patch(dex: &[u8], edits: &[(usize, &Class)]) -> R<Option<Vec<u8>>> {
    let r = read::Reader::new(dex)?;
    let p = Pools::from_reader(&r);
    let dx = crate::dex::Dex::parse(dex)?;
    let mut out = dex.to_vec();
    for &(ci, new) in edits {
        let Ok(orig) = r.class(ci) else {
            return Ok(None);
        };
        if !shell_eq(&orig, new) {
            return Ok(None); // fields/annotations/signatures changed: needs a full rebuild
        }
        let mut col = Collect::default();
        col.class(new);
        if !refs_present(&col, &p) {
            return Ok(None); // edit introduces a new pool entry: needs a full rebuild
        }
        let cd = dx.class_data(ci)?;
        if cd.methods.len() != orig.methods.len() || new.methods.len() != orig.methods.len() {
            return Ok(None);
        }
        for ((om, nm), dm) in orig.methods.iter().zip(&new.methods).zip(&cd.methods) {
            let (oc, nc) = match (&om.code, &nm.code) {
                (Some(a), Some(b)) if a != b => (a, b),
                (Some(_), Some(_)) | (None, None) => continue,
                _ => return Ok(None), // a method gained or lost its body
            };
            let what = || {
                format!(
                    "{}->{}",
                    String::from_utf8_lossy(&new.name),
                    String::from_utf8_lossy(&nm.name)
                )
            };
            let ao = assemble(oc, &p, &what)?;
            let an = assemble(nc, &p, &what)?;
            // only the instruction bytes may differ; registers/outs/tries/handlers/debug and the
            // total length must match, so the surrounding code_item and all other offsets are kept
            if (ao.outs, &ao.tries, &ao.handlers, &ao.events, ao.insns.len())
                != (an.outs, &an.tries, &an.handlers, &an.events, an.insns.len())
            {
                return Ok(None);
            }
            let Some(code) = &dm.code else {
                return Ok(None);
            };
            let off = code.insns.as_ptr() as usize - dex.as_ptr() as usize;
            let old: Vec<u8> = ao.insns.iter().flat_map(|u| u.to_le_bytes()).collect();
            // the original must round-trip to its own bytes, or we don't trust the overwrite
            if code.insns != old {
                return Ok(None);
            }
            let new_bytes: Vec<u8> = an.insns.iter().flat_map(|u| u.to_le_bytes()).collect();
            out[off..off + new_bytes.len()].copy_from_slice(&new_bytes);
        }
    }
    seal(&mut out);
    Ok(Some(out))
}

/// Everything about two class models matches except the instruction bodies of methods.
fn shell_eq(a: &Class, b: &Class) -> bool {
    a.name == b.name
        && a.access == b.access
        && a.sup == b.sup
        && a.ifaces == b.ifaces
        && a.source == b.source
        && a.annos == b.annos
        && a.fields == b.fields
        && a.methods.len() == b.methods.len()
        && a.methods.iter().zip(&b.methods).all(|(x, y)| {
            x.name == y.name
                && x.proto == y.proto
                && x.access == y.access
                && x.hidden == y.hidden
                && x.annos == y.annos
                && x.param_annos == y.param_annos
        })
}

/// Every reference an edited class makes is already present in the DEX's pools.
fn refs_present(c: &Collect, p: &Pools) -> bool {
    c.strings.iter().all(|x| p.si.contains_key(x))
        && c.types.iter().all(|x| p.ti.contains_key(x))
        && c.protos.iter().all(|x| p.pi.contains_key(x))
        && c.fields.iter().all(|x| p.fi.contains_key(x))
        && c.methods.iter().all(|x| p.mi.contains_key(x))
        && c.handles.iter().all(|x| p.hi.contains_key(x))
        && c.sites.iter().all(|x| p.ci.contains_key(x))
}

/// Recompute the header's SHA-1 signature and Adler-32 checksum.
pub fn seal(f: &mut [u8]) {
    use sha1::{Digest, Sha1};
    let sig = Sha1::digest(&f[32..]);
    f[12..32].copy_from_slice(&sig);
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in f[12..].chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    f[8..12].copy_from_slice(&(b << 16 | a).to_le_bytes());
}
