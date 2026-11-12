//! DEX bytes -> class model. Self-contained so the query paths stay untouched.

use super::model::*;
use crate::bytecode::{self, Fmt};
use crate::dex::{sleb128, uleb128};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type R<T> = Result<T, String>;

fn u32_at(d: &[u8], o: usize) -> R<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| format!("read past end at {o:#x}"))
}
fn u16_at(d: &[u8], o: usize) -> R<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| format!("read past end at {o:#x}"))
}

pub struct Reader<'a> {
    d: &'a [u8],
    strings: Vec<S>,
    types: Vec<S>,
    protos: Vec<Proto>,
    fields: Vec<FieldRef>,
    methods: Vec<MethodRef>,
    handles: Vec<Handle>,
    call_sites: Vec<Arc<Vec<Value>>>,
    class_defs: usize,
    class_count: usize,
    /// hiddenapi_class_data section, 0 when the file has none
    hidden: usize,
}

/// Every class of a DEX file, in class_def order.
pub fn read(d: &[u8]) -> R<Vec<Class>> {
    let r = Reader::new(d)?;
    (0..r.class_count).map(|i| r.class(i)).collect()
}

impl<'a> Reader<'a> {
    pub fn new(d: &'a [u8]) -> R<Self> {
        if d.len() < 0x70 || &d[..4] != b"dex\n" {
            return Err("not a DEX file".into());
        }
        let tab = |o: usize| -> R<(usize, usize)> {
            Ok((u32_at(d, o)? as usize, u32_at(d, o + 4)? as usize))
        };
        let (ns, so) = tab(0x38)?;
        let strings = (0..ns)
            .map(|i| {
                let mut o = u32_at(d, so + i * 4)? as usize;
                uleb128(d, &mut o)?;
                let end = d
                    .get(o..)
                    .and_then(|t| t.iter().position(|&b| b == 0))
                    .ok_or("unterminated string")?;
                Ok(s(&d[o..o + end]))
            })
            .collect::<R<Vec<S>>>()?;
        let st = |i: u32| {
            strings
                .get(i as usize)
                .cloned()
                .ok_or_else(|| format!("bad string id {i}"))
        };
        let (nt, to) = tab(0x40)?;
        let types = (0..nt)
            .map(|i| st(u32_at(d, to + i * 4)?))
            .collect::<R<Vec<S>>>()?;
        let ty = |i: u32| {
            types
                .get(i as usize)
                .cloned()
                .ok_or_else(|| format!("bad type id {i}"))
        };
        let (np, po) = tab(0x48)?;
        let protos = (0..np)
            .map(|i| {
                let o = po + i * 12;
                let list = u32_at(d, o + 8)? as usize;
                let params = if list == 0 {
                    vec![]
                } else {
                    (0..u32_at(d, list)? as usize)
                        .map(|k| ty(u16_at(d, list + 4 + k * 2)? as u32))
                        .collect::<R<_>>()?
                };
                Ok(Proto {
                    ret: ty(u32_at(d, o + 4)?)?,
                    params,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let (nf, fo) = tab(0x50)?;
        let fields = (0..nf)
            .map(|i| {
                let o = fo + i * 8;
                Ok(FieldRef {
                    class: ty(u16_at(d, o)? as u32)?,
                    ty: ty(u16_at(d, o + 2)? as u32)?,
                    name: st(u32_at(d, o + 4)?)?,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let (nm, mo) = tab(0x58)?;
        let methods = (0..nm)
            .map(|i| {
                let o = mo + i * 8;
                let proto = protos
                    .get(u16_at(d, o + 2)? as usize)
                    .cloned()
                    .ok_or("bad proto id")?;
                Ok(MethodRef {
                    class: ty(u16_at(d, o)? as u32)?,
                    proto,
                    name: st(u32_at(d, o + 4)?)?,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let (class_count, class_defs) = tab(0x60)?;
        let mut r = Reader {
            d,
            strings,
            types,
            protos,
            fields,
            methods,
            handles: vec![],
            call_sites: vec![],
            class_defs,
            class_count,
            hidden: 0,
        };
        // call sites and method handles are only listed in the map
        let map = u32_at(d, 0x34)? as usize;
        let mut sections = (0, 0, 0, 0);
        let entries = if map == 0 {
            0
        } else {
            u32_at(d, map)? as usize
        };
        for k in 0..entries {
            let o = map + 4 + k * 12;
            match u16_at(d, o)? {
                7 => {
                    (sections.0, sections.1) =
                        (u32_at(d, o + 4)? as usize, u32_at(d, o + 8)? as usize)
                }
                8 => {
                    (sections.2, sections.3) =
                        (u32_at(d, o + 4)? as usize, u32_at(d, o + 8)? as usize)
                }
                0xf000 => r.hidden = u32_at(d, o + 8)? as usize,
                _ => {}
            }
        }
        for i in 0..sections.2 {
            let o = sections.3 + i * 8;
            let kind = u16_at(d, o)?;
            let id = u16_at(d, o + 4)? as usize;
            let member = if kind <= 3 {
                Member::Field(
                    r.fields
                        .get(id)
                        .cloned()
                        .ok_or("bad field in method handle")?,
                )
            } else {
                Member::Method(
                    r.methods
                        .get(id)
                        .cloned()
                        .ok_or("bad method in method handle")?,
                )
            };
            r.handles.push(Handle { kind, member });
        }
        for i in 0..sections.0 {
            let mut o = u32_at(d, sections.1 + i * 4)? as usize;
            let v = r.array(&mut o)?;
            r.call_sites.push(Arc::new(v));
        }
        Ok(r)
    }

    fn st(&self, i: u32) -> R<S> {
        self.strings
            .get(i as usize)
            .cloned()
            .ok_or_else(|| format!("bad string id {i}"))
    }
    fn ty(&self, i: u32) -> R<S> {
        self.types
            .get(i as usize)
            .cloned()
            .ok_or_else(|| format!("bad type id {i}"))
    }
    fn opt_st(&self, i: u32) -> R<Option<S>> {
        if i == u32::MAX {
            Ok(None)
        } else {
            self.st(i).map(Some)
        }
    }
    /// uleb128p1 string index
    fn p1(&self, o: &mut usize) -> R<Option<S>> {
        match uleb128(self.d, o)? {
            0 => Ok(None),
            v => self.st(v - 1).map(Some),
        }
    }

    fn array(&self, o: &mut usize) -> R<Vec<Value>> {
        let n = uleb128(self.d, o)?;
        (0..n).map(|_| self.value(o, 0)).collect()
    }

    fn value(&self, o: &mut usize, depth: u32) -> R<Value> {
        if depth > 64 {
            return Err("encoded value nesting too deep".into());
        }
        let d = self.d;
        let h = *d.get(*o).ok_or("value past end")?;
        *o += 1;
        let (t, arg) = (h & 0x1f, (h >> 5) as usize);
        let mut raw = |n: usize| -> R<u64> {
            let b = d.get(*o..*o + n).ok_or("value past end")?;
            *o += n;
            Ok(b.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64))
        };
        let sx = |v: u64, n: usize| ((v << (64 - 8 * n)) as i64) >> (64 - 8 * n);
        let idx = |v: u64| v as u32;
        Ok(match t {
            0x00 => Value::Byte(sx(raw(1)?, 1) as i8),
            0x02 => Value::Short(sx(raw(arg + 1)?, arg + 1) as i16),
            0x03 => Value::Char(raw(arg + 1)? as u16),
            0x04 => Value::Int(sx(raw(arg + 1)?, arg + 1) as i32),
            0x06 => Value::Long(sx(raw(arg + 1)?, arg + 1)),
            0x10 => Value::Float((raw(arg + 1)? << (8 * (3 - arg.min(3)))) as u32),
            0x11 => Value::Double(raw(arg + 1)? << (8 * (7 - arg.min(7)))),
            0x15 => Value::MethodType(
                self.protos
                    .get(idx(raw(arg + 1)?) as usize)
                    .cloned()
                    .ok_or("bad proto")?,
            ),
            0x16 => Value::MethodHandle(Box::new(
                self.handles
                    .get(idx(raw(arg + 1)?) as usize)
                    .cloned()
                    .ok_or("bad method handle")?,
            )),
            0x17 => Value::Str(self.st(idx(raw(arg + 1)?))?),
            0x18 => Value::Type(self.ty(idx(raw(arg + 1)?))?),
            0x19 => Value::Field(
                self.fields
                    .get(idx(raw(arg + 1)?) as usize)
                    .cloned()
                    .ok_or("bad field")?,
            ),
            0x1a => Value::Method(
                self.methods
                    .get(idx(raw(arg + 1)?) as usize)
                    .cloned()
                    .ok_or("bad method")?,
            ),
            0x1b => Value::Enum(
                self.fields
                    .get(idx(raw(arg + 1)?) as usize)
                    .cloned()
                    .ok_or("bad field")?,
            ),
            0x1c => {
                let n = uleb128(d, o)?;
                Value::Array((0..n).map(|_| self.value(o, depth + 1)).collect::<R<_>>()?)
            }
            0x1d => Value::Anno(self.anno(o, 0, depth + 1)?),
            0x1e => Value::Null,
            0x1f => Value::Bool(arg != 0),
            t => return Err(format!("bad encoded value type {t:#x}")),
        })
    }

    fn anno(&self, o: &mut usize, vis: u8, depth: u32) -> R<Anno> {
        let ty = self.ty(uleb128(self.d, o)?)?;
        let n = uleb128(self.d, o)?;
        let mut elems = Vec::with_capacity(n.min(64) as usize);
        for _ in 0..n {
            let name = self.st(uleb128(self.d, o)?)?;
            elems.push((name, self.value(o, depth)?));
        }
        Ok(Anno { vis, ty, elems })
    }

    fn anno_set(&self, off: usize) -> R<Vec<Anno>> {
        if off == 0 {
            return Ok(vec![]);
        }
        (0..u32_at(self.d, off)? as usize)
            .map(|k| {
                let mut o = u32_at(self.d, off + 4 + k * 4)? as usize;
                let vis = *self.d.get(o).ok_or("annotation past end")?;
                o += 1;
                self.anno(&mut o, vis, 0)
            })
            .collect()
    }

    /// The DEX's id tables, already in model form and sorted order, for reusing as write pools.
    #[allow(clippy::type_complexity)]
    pub(super) fn pool_tables(
        &self,
    ) -> (
        &[S],
        &[S],
        &[Proto],
        &[FieldRef],
        &[MethodRef],
        &[Handle],
        &[Arc<Vec<Value>>],
    ) {
        (
            &self.strings,
            &self.types,
            &self.protos,
            &self.fields,
            &self.methods,
            &self.handles,
            &self.call_sites,
        )
    }

    pub fn class(&self, i: usize) -> R<Class> {
        let d = self.d;
        let o = self.class_defs + i * 32;
        let name = self.ty(u32_at(d, o)?)?;
        let sup = match u32_at(d, o + 8)? {
            u32::MAX => None,
            t => Some(self.ty(t)?),
        };
        let il = u32_at(d, o + 12)? as usize;
        let ifaces = if il == 0 {
            vec![]
        } else {
            (0..u32_at(d, il)? as usize)
                .map(|k| self.ty(u16_at(d, il + 4 + k * 2)? as u32))
                .collect::<R<_>>()?
        };
        let mut c = Class {
            name,
            access: u32_at(d, o + 4)?,
            sup,
            ifaces,
            source: self.opt_st(u32_at(d, o + 16)?)?,
            annos: vec![],
            fields: vec![],
            methods: vec![],
        };

        // annotations by field / method index
        let dir = u32_at(d, o + 20)? as usize;
        let (mut fa, mut ma, mut pa) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        if dir != 0 {
            c.annos = self.anno_set(u32_at(d, dir)? as usize)?;
            let (nf, nm, np) = (
                u32_at(d, dir + 4)? as usize,
                u32_at(d, dir + 8)? as usize,
                u32_at(d, dir + 12)? as usize,
            );
            let mut p = dir + 16;
            for _ in 0..nf {
                fa.insert(u32_at(d, p)?, self.anno_set(u32_at(d, p + 4)? as usize)?);
                p += 8;
            }
            for _ in 0..nm {
                ma.insert(u32_at(d, p)?, self.anno_set(u32_at(d, p + 4)? as usize)?);
                p += 8;
            }
            for _ in 0..np {
                let list = u32_at(d, p + 4)? as usize;
                let sets = if list == 0 {
                    vec![]
                } else {
                    (0..u32_at(d, list)? as usize)
                        .map(|k| self.anno_set(u32_at(d, list + 4 + k * 4)? as usize))
                        .collect::<R<Vec<_>>>()?
                };
                pa.insert(u32_at(d, p)?, sets);
                p += 8;
            }
        }

        let mut statics = {
            let mut sv = u32_at(d, o + 28)? as usize;
            if sv == 0 {
                vec![]
            } else {
                self.array(&mut sv)?
            }
        }
        .into_iter();

        // hiddenapi flags: one uleb128 per member, in class_data order
        let mut hid = match self.hidden {
            0 => 0,
            h => match u32_at(d, h + 4 + i * 4)? as usize {
                0 => 0,
                rel => h + rel,
            },
        };
        let mut hidden = || -> R<Option<u32>> {
            if hid == 0 {
                Ok(None)
            } else {
                uleb128(d, &mut hid).map(Some)
            }
        };

        let mut off = u32_at(d, o + 24)? as usize;
        if off != 0 {
            let counts = [
                uleb128(d, &mut off)?,
                uleb128(d, &mut off)?,
                uleb128(d, &mut off)?,
                uleb128(d, &mut off)?,
            ];
            for (k, &count) in counts[..2].iter().enumerate() {
                let mut idx = 0u32;
                for _ in 0..count {
                    idx += uleb128(d, &mut off)?;
                    let access = uleb128(d, &mut off)?;
                    let f = self.fields.get(idx as usize).ok_or("bad field id")?;
                    let init = if k == 0 { statics.next() } else { None };
                    c.fields.push(Field {
                        name: f.name.clone(),
                        ty: f.ty.clone(),
                        access,
                        hidden: hidden()?,
                        init,
                        annos: fa.remove(&idx).unwrap_or_default(),
                    });
                }
            }
            for &count in &counts[2..] {
                let mut idx = 0u32;
                for _ in 0..count {
                    idx += uleb128(d, &mut off)?;
                    let access = uleb128(d, &mut off)?;
                    let code_off = uleb128(d, &mut off)? as usize;
                    let m = self.methods.get(idx as usize).ok_or("bad method id")?;
                    let mut method = Method {
                        name: m.name.clone(),
                        proto: m.proto.clone(),
                        access,
                        hidden: hidden()?,
                        code: None,
                        annos: ma.remove(&idx).unwrap_or_default(),
                        param_annos: None,
                    };
                    if let Some(sets) = pa.remove(&idx) {
                        let mut sets = sets;
                        sets.resize(method.proto.params.len().max(sets.len()), vec![]);
                        if sets.iter().any(|s| !s.is_empty()) {
                            method.param_annos = Some(sets);
                        }
                    }
                    if code_off != 0 {
                        method.code = Some(
                            self.code(code_off, method.proto.params.len())
                                .map_err(|e| {
                                    format!(
                                        "{}->{}: {e}",
                                        String::from_utf8_lossy(&c.name),
                                        String::from_utf8_lossy(&method.name)
                                    )
                                })?,
                        );
                    }
                    c.methods.push(method);
                }
            }
        }
        Ok(c)
    }

    fn code(&self, co: usize, nparams: usize) -> R<Code> {
        let d = self.d;
        let registers = u16_at(d, co)?;
        let ntries = u16_at(d, co + 6)? as usize;
        let debug_off = u32_at(d, co + 8)? as usize;
        let units = u32_at(d, co + 12)? as usize;
        let insns = d.get(co + 16..co + 16 + units * 2).ok_or("code past end")?;
        let unit = |pc: usize| -> R<u32> {
            insns
                .get(pc * 2..pc * 2 + 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
                .ok_or_else(|| format!("payload past end at {pc:#x}"))
        };
        let decoded = bytecode::decode(insns)?;

        let mut labels = BTreeSet::new();
        // offset -> item, merged in address order (payloads are skipped by decode)
        let mut at: BTreeMap<u32, Vec<Item>> = BTreeMap::new();
        let mut payload_users: BTreeMap<u32, u32> = BTreeMap::new();
        for i in &decoded {
            let (_, f, kind) = bytecode::info(i.op).unwrap();
            let mut ins = Ins {
                op: i.op,
                regs: i.regs,
                n: i.nregs,
                lit: i.lit,
                target: 0,
                r: Ref::None,
                proto: None,
            };
            match f {
                Fmt::F10t | Fmt::F20t | Fmt::F30t | Fmt::F21t | Fmt::F22t | Fmt::F31t => {
                    let t = (i.off as i64 + i.lit) as u32;
                    ins.target = t;
                    ins.lit = 0;
                    labels.insert(t);
                    if f == Fmt::F31t {
                        payload_users.insert(t, i.off);
                    }
                }
                _ => {}
            }
            ins.r = match kind {
                bytecode::Ref::None => Ref::None,
                bytecode::Ref::String => Ref::Str(self.st(i.idx)?),
                bytecode::Ref::Type => Ref::Type(self.ty(i.idx)?),
                bytecode::Ref::Field => Ref::Field(
                    self.fields
                        .get(i.idx as usize)
                        .cloned()
                        .ok_or("bad field id")?,
                ),
                bytecode::Ref::Method => Ref::Method(
                    self.methods
                        .get(i.idx as usize)
                        .cloned()
                        .ok_or("bad method id")?,
                ),
                bytecode::Ref::Proto => Ref::Proto(
                    self.protos
                        .get(i.idx as usize)
                        .cloned()
                        .ok_or("bad proto id")?,
                ),
                bytecode::Ref::CallSite => Ref::CallSite(
                    self.call_sites
                        .get(i.idx as usize)
                        .cloned()
                        .ok_or("bad call site id")?,
                ),
                bytecode::Ref::MethodHandle => Ref::Handle(
                    self.handles
                        .get(i.idx as usize)
                        .cloned()
                        .ok_or("bad method handle id")?,
                ),
            };
            if matches!(f, Fmt::F45cc | Fmt::F4rcc) {
                ins.proto = Some(
                    self.protos
                        .get(i.idx2 as usize)
                        .cloned()
                        .ok_or("bad proto id")?,
                );
            }
            at.entry(i.off).or_default().push(Item::Ins(ins));
        }
        for (&p, &user) in &payload_users {
            let p = p as usize;
            let item = match unit(p)? {
                0x0100 => {
                    let n = unit(p + 1)? as usize;
                    let first = (unit(p + 2)? | unit(p + 3)? << 16) as i32;
                    let targets = (0..n)
                        .map(|k| {
                            Ok((user as i64
                                + (unit(p + 4 + k * 2)? | unit(p + 5 + k * 2)? << 16) as i32 as i64)
                                as u32)
                        })
                        .collect::<R<Vec<_>>>()?;
                    labels.extend(targets.iter().copied());
                    Item::Packed { first, targets }
                }
                0x0200 => {
                    let n = unit(p + 1)? as usize;
                    let key = |k: usize| -> R<i32> {
                        Ok((unit(p + 2 + k * 2)? | unit(p + 3 + k * 2)? << 16) as i32)
                    };
                    let tgt = |k: usize| -> R<u32> {
                        Ok((user as i64
                            + (unit(p + 2 + n * 2 + k * 2)? | unit(p + 3 + n * 2 + k * 2)? << 16)
                                as i32 as i64) as u32)
                    };
                    let v = (0..n)
                        .map(|k| Ok((key(k)?, tgt(k)?)))
                        .collect::<R<Vec<_>>>()?;
                    labels.extend(v.iter().map(|x| x.1));
                    Item::Sparse(v)
                }
                0x0300 => {
                    let width = unit(p + 1)? as u16;
                    let n = (unit(p + 2)? | unit(p + 3)? << 16) as usize;
                    let start = (p + 4) * 2;
                    let data = insns
                        .get(start..start + n * width as usize)
                        .ok_or("array data past end")?
                        .to_vec();
                    Item::Array { width, data }
                }
                w => return Err(format!("bad payload {w:#x} at {p:#x}")),
            };
            at.entry(p as u32).or_default().push(item);
            // the nop just before an aligned payload is padding; the writer re-adds it
            if p.is_multiple_of(2)
                && p > 0
                && let Some(v) = at.get_mut(&(p as u32 - 1))
                && matches!(v.as_slice(), [Item::Ins(i)] if i.op == 0)
            {
                at.remove(&(p as u32 - 1));
            }
        }

        let mut tries = Vec::with_capacity(ntries);
        for t in crate::dex::parse_tries(d, co, units)? {
            let end = t.start + t.count;
            labels.insert(t.start);
            labels.insert(end);
            let handlers = t
                .handlers
                .into_iter()
                .map(|(ty, a)| {
                    labels.insert(a);
                    Ok((ty.map(|x| self.ty(x)).transpose()?, a))
                })
                .collect::<R<Vec<_>>>()?;
            tries.push(Try {
                start: t.start,
                end,
                handlers,
            });
        }

        // debug events by address
        let mut events: BTreeMap<u32, Vec<Item>> = BTreeMap::new();
        let mut param_names = vec![];
        if debug_off != 0 {
            let mut o = debug_off;
            let mut line = uleb128(d, &mut o)?;
            let n = uleb128(d, &mut o)? as usize;
            for _ in 0..n {
                param_names.push(self.p1(&mut o)?);
            }
            let mut addr = 0u32;
            loop {
                let op = *d.get(o).ok_or("debug info past end")?;
                o += 1;
                let ev = match op {
                    0 => break,
                    1 => {
                        addr += uleb128(d, &mut o)?;
                        continue;
                    }
                    2 => {
                        line = line.wrapping_add(sleb128(d, &mut o)? as u32);
                        continue;
                    }
                    3 | 4 => {
                        let reg = uleb128(d, &mut o)? as u16;
                        let name = self.p1(&mut o)?;
                        // the type is an index into type_ids, not string_ids
                        let ty = match uleb128(d, &mut o)? {
                            0 => None,
                            v => Some(self.ty(v - 1)?),
                        };
                        let sig = if op == 4 { self.p1(&mut o)? } else { None };
                        Item::Local { reg, name, ty, sig }
                    }
                    5 => Item::EndLocal(uleb128(d, &mut o)? as u16),
                    6 => Item::Restart(uleb128(d, &mut o)? as u16),
                    7 => Item::Prologue,
                    8 => Item::Epilogue,
                    9 => Item::SetFile(self.p1(&mut o)?),
                    _ => {
                        let adj = (op - 0x0a) as i32;
                        line = line.wrapping_add((adj % 15 - 4) as u32);
                        addr += (adj / 15) as u32;
                        Item::Line(line)
                    }
                };
                events.entry(addr).or_default().push(ev);
            }
        }
        // debug info with no events and no names says nothing; the text form cannot carry it
        param_names.resize(nparams, None);
        let debug =
            debug_off != 0 && (!events.is_empty() || param_names.iter().any(|n| n.is_some()));

        // shrinkers leave line entries inside an instruction; Smali can only place them between
        let starts: BTreeSet<u32> = at.keys().copied().chain([units as u32]).collect();
        if events.keys().any(|a| !starts.contains(a)) {
            let mut snapped: BTreeMap<u32, Vec<Item>> = BTreeMap::new();
            for (a, ev) in std::mem::take(&mut events) {
                let to = starts.range(a..).next().copied().unwrap_or(a);
                snapped.entry(to).or_default().extend(ev);
            }
            events = snapped;
        }

        let mut items = Vec::with_capacity(decoded.len() + labels.len());
        let mut keys: BTreeSet<u32> = at.keys().copied().collect();
        keys.extend(labels.iter().copied());
        keys.extend(events.keys().copied());
        for k in keys {
            if labels.contains(&k) {
                items.push(Item::Label(k));
            }
            if let Some(ev) = events.remove(&k) {
                items.extend(ev);
            }
            if let Some(v) = at.remove(&k) {
                items.extend(v);
            }
        }
        Ok(Code {
            registers,
            items,
            tries,
            debug,
            param_names: if debug {
                param_names
            } else {
                vec![None; nparams]
            },
        })
    }
}
