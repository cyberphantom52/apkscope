//! Minimal DEX table reader. Spec: https://source.android.com/docs/core/runtime/dex-format

use crate::{Error, Result};

pub struct Dex<'a> {
    data: &'a [u8],
    string_ids: usize,
    type_ids: usize,
    proto_ids: usize,
    field_ids: usize,
    method_ids: usize,
    class_defs: usize,
    pub class_count: usize,
    /// (descriptor hash, class_def index) sorted, built on first lookup; the name itself is
    /// compared on lookup, so no copy of every descriptor is kept
    by_name: std::sync::OnceLock<Vec<(u64, u32)>>,
    nesting: std::sync::OnceLock<Nesting>,
}

/// Inner classes by their outer class, from the `Outer$Inner` naming convention.
pub struct Nesting {
    pub parent: Vec<Option<usize>>,
    pub kids: Vec<Vec<usize>>,
}

pub struct Method<'a> {
    pub method_idx: u32,
    pub access: u32,
    /// bytecode in 16-bit code units; None for abstract/native
    pub code: Option<Code<'a>>,
}

pub struct Code<'a> {
    pub registers: u16,
    pub ins: u16,
    pub insns: &'a [u8],
    pub tries: Vec<Try>,
}

/// Instructions in [start, start+count) code units are covered by `handlers`.
pub struct Try {
    pub start: u32,
    pub count: u32,
    /// (exception type_idx or None for catch-all, handler address)
    pub handlers: Vec<(Option<u32>, u32)>,
}

pub struct ClassDef {
    pub access: u32,
    pub class: String,
    pub superclass: Option<String>,
    pub interfaces: Vec<String>,
    /// SourceFile attribute (`Foo.java`), a naming hint for obfuscated classes
    pub source_file: Option<String>,
}

pub struct ClassData<'a> {
    /// (field_idx, access); static fields first
    pub fields: Vec<(u32, u32)>,
    pub methods: Vec<Method<'a>>,
}

type R<T> = Result<T>;

fn bytes<const N: usize>(d: &[u8], off: usize) -> R<[u8; N]> {
    d.get(off..off + N)
        .map(|b| b.try_into().unwrap())
        .ok_or_else(|| Error::dex(format!("read past end at {off:#x}")))
}
fn u32_at(d: &[u8], off: usize) -> R<u32> {
    bytes(d, off).map(u32::from_le_bytes)
}
fn u16_at(d: &[u8], off: usize) -> R<u16> {
    bytes(d, off).map(u16::from_le_bytes)
}

pub fn uleb128(d: &[u8], off: &mut usize) -> R<u32> {
    let mut v = 0u32;
    for shift in (0..35).step_by(7) {
        let b = *d.get(*off).ok_or_else(|| Error::dex("uleb128 past end"))?;
        *off += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::dex("uleb128 too long"))
}

pub fn sleb128(d: &[u8], off: &mut usize) -> R<i32> {
    let start = *off;
    let v = uleb128(d, off)?;
    let bits = 7 * (*off - start) as u32;
    // sign-extend from the last encoded bit
    Ok(if bits < 32 {
        ((v << (32 - bits)) as i32) >> (32 - bits)
    } else {
        v as i32
    })
}

fn type_list(d: &[u8], off: usize) -> R<Vec<u32>> {
    if off == 0 {
        return Ok(vec![]);
    }
    (0..u32_at(d, off)? as usize)
        .map(|i| u16_at(d, off + 4 + i * 2).map(u32::from))
        .collect()
}

pub fn parse_tries(d: &[u8], code_off: usize, units: usize) -> R<Vec<Try>> {
    let n = u16_at(d, code_off + 6)? as usize;
    // try_items are 4-byte aligned after insns
    let tries = code_off + 16 + units * 2 + (units % 2) * 2;
    let list = tries + n * 8;
    let mut out = Vec::with_capacity(n);
    for t in 0..n {
        let o = tries + t * 8;
        let mut h = list + u16_at(d, o + 6)? as usize;
        let size = sleb128(d, &mut h)?;
        let mut handlers = Vec::new();
        for _ in 0..size.unsigned_abs() {
            let ty = uleb128(d, &mut h)?;
            handlers.push((Some(ty), uleb128(d, &mut h)?));
        }
        if size <= 0 {
            handlers.push((None, uleb128(d, &mut h)?));
        }
        out.push(Try {
            start: u32_at(d, o)?,
            count: u16_at(d, o + 4)? as u32,
            handlers,
        });
    }
    Ok(out)
}

impl<'a> Dex<'a> {
    pub fn parse(data: &'a [u8]) -> R<Self> {
        if data.len() < 0x70 || &data[..4] != b"dex\n" {
            return Err(Error::dex("not a DEX file"));
        }
        let dex = Dex {
            data,
            string_ids: u32_at(data, 0x3C)? as usize,
            type_ids: u32_at(data, 0x44)? as usize,
            proto_ids: u32_at(data, 0x4C)? as usize,
            field_ids: u32_at(data, 0x54)? as usize,
            method_ids: u32_at(data, 0x5C)? as usize,
            class_count: u32_at(data, 0x60)? as usize,
            class_defs: u32_at(data, 0x64)? as usize,
            by_name: std::sync::OnceLock::new(),
            nesting: std::sync::OnceLock::new(),
        };
        // validate table bounds once so lookups only fail on corrupt indices
        if dex.class_defs + dex.class_count * 32 > data.len() {
            return Err(Error::dex("class_defs out of bounds"));
        }
        Ok(dex)
    }

    /// Sizes of the string, type, field and method tables.
    pub fn counts(&self) -> [usize; 4] {
        [0x38, 0x40, 0x50, 0x58].map(|o| u32_at(self.data, o).unwrap_or(0) as usize)
    }

    pub fn string(&self, idx: u32) -> R<String> {
        let mut off = u32_at(self.data, self.string_ids + idx as usize * 4)? as usize;
        uleb128(self.data, &mut off)?; // utf16 length, unused
        let end = self.data[off..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::dex("unterminated string"))?;
        Ok(mutf8(&self.data[off..off + end]))
    }

    /// Raw MUTF-8 bytes of string `idx`, without copying.
    pub fn string_bytes(&self, idx: u32) -> R<&'a [u8]> {
        let mut off = u32_at(self.data, self.string_ids + idx as usize * 4)? as usize;
        uleb128(self.data, &mut off)?;
        let end = self
            .data
            .get(off..)
            .and_then(|d| d.iter().position(|&b| b == 0))
            .ok_or_else(|| Error::dex("unterminated string"))?;
        Ok(&self.data[off..off + end])
    }

    pub fn type_name(&self, idx: u32) -> R<String> {
        self.string(u32_at(self.data, self.type_ids + idx as usize * 4)?)
    }

    pub fn class_descriptor(&self, i: usize) -> R<String> {
        self.type_name(u32_at(self.data, self.class_defs + i * 32)?)
    }

    /// (class, type, name) descriptors of a field_id
    pub fn field_parts(&self, idx: u32) -> R<(String, String, String)> {
        let off = self.field_ids + idx as usize * 8;
        let class = self.type_name(u16_at(self.data, off)? as u32)?;
        let ty = self.type_name(u16_at(self.data, off + 2)? as u32)?;
        Ok((class, ty, self.string(u32_at(self.data, off + 4)?)?))
    }

    /// `Lcls;->name:Type`
    pub fn field_ref(&self, idx: u32) -> R<String> {
        let (class, ty, name) = self.field_parts(idx)?;
        Ok(format!("{class}->{name}:{ty}"))
    }

    /// (return type, param types) descriptors of a proto_id
    pub fn proto_parts(&self, idx: u32) -> R<(String, Vec<String>)> {
        let off = self.proto_ids + idx as usize * 12;
        let ret = self.type_name(u32_at(self.data, off + 4)?)?;
        let params = type_list(self.data, u32_at(self.data, off + 8)? as usize)?;
        Ok((
            ret,
            params
                .into_iter()
                .map(|t| self.type_name(t))
                .collect::<R<_>>()?,
        ))
    }

    /// `(params)Ret`
    pub fn proto(&self, idx: u32) -> R<String> {
        let (ret, params) = self.proto_parts(idx)?;
        Ok(format!("({}){ret}", params.concat()))
    }

    /// (class descriptor, name, proto_idx) of a method_id
    pub fn method_parts(&self, idx: u32) -> R<(String, String, u32)> {
        let off = self.method_ids + idx as usize * 8;
        let class = self.type_name(u16_at(self.data, off)? as u32)?;
        Ok((
            class,
            self.string(u32_at(self.data, off + 4)?)?,
            u16_at(self.data, off + 2)? as u32,
        ))
    }

    /// `Lcls;->name(params)Ret`
    pub fn method_ref(&self, idx: u32) -> R<String> {
        let (class, name, proto) = self.method_parts(idx)?;
        Ok(format!("{class}->{name}{}", self.proto(proto)?))
    }

    pub fn find_class(&self, desc: &str) -> Option<usize> {
        let table = self.by_name.get_or_init(|| {
            let mut v: Vec<(u64, u32)> = (0..self.class_count)
                .filter_map(|i| Some((name_hash(&self.class_descriptor(i).ok()?), i as u32)))
                .collect();
            v.sort_unstable();
            v
        });
        let h = name_hash(desc);
        let from = table.partition_point(|e| e.0 < h);
        table[from..]
            .iter()
            .take_while(|e| e.0 == h)
            .map(|e| e.1 as usize)
            .find(|&i| self.class_descriptor(i).is_ok_and(|d| d == desc))
    }

    pub fn nesting(&self) -> &Nesting {
        self.nesting.get_or_init(|| {
            let mut parent = vec![None; self.class_count];
            let mut kids = vec![vec![]; self.class_count];
            for (c, p) in parent.iter_mut().enumerate() {
                *p = self.find_parent(c);
                if let Some(o) = *p {
                    kids[o].push(c);
                }
            }
            Nesting { parent, kids }
        })
    }

    /// Nearest enclosing class of `c` that exists in this dex: La/B$C$D; -> La/B$C; -> La/B;
    fn find_parent(&self, c: usize) -> Option<usize> {
        let desc = self.class_descriptor(c).ok()?;
        let body = desc.trim_end_matches(';');
        body.char_indices()
            .rev()
            .filter(|&(i, ch)| ch == '$' && i > 0)
            .find_map(|(i, _)| {
                self.find_class(&format!("{};", &body[..i]))
                    .filter(|&o| o != c)
            })
    }

    /// `nesting().parent[c]`, without building the table for every class when it isn't there.
    pub fn parent_of(&self, c: usize) -> Option<usize> {
        match self.nesting.get() {
            Some(n) => n.parent[c],
            None => self.find_parent(c),
        }
    }

    /// Every class nested (at any depth) inside `c` by the `Outer$Inner` rule, in class order:
    /// one pass over the class names instead of the whole nesting table.
    pub fn descendants(&self, c: usize) -> Vec<usize> {
        if let Some(n) = self.nesting.get() {
            let mut out = Vec::new();
            let mut stack = vec![c];
            while let Some(x) = stack.pop() {
                out.extend(n.kids[x].iter().copied());
                stack.extend(n.kids[x].iter().copied());
            }
            out.sort_unstable();
            return out;
        }
        let Ok(desc) = self.class_descriptor(c) else {
            return vec![];
        };
        let prefix = format!("{}$", desc.trim_end_matches(';'));
        // an ancestor chain leads back to `c` exactly when the name starts with `c$`
        // (raw bytes equal UTF-8 except for NUL / characters above U+FFFF: decode those)
        let starts = |d: &[u8]| {
            d.starts_with(prefix.as_bytes())
                || (std::str::from_utf8(d).is_err() && mutf8(d).starts_with(&prefix))
        };
        (0..self.class_count)
            .filter(|&k| k != c && self.class_desc_bytes(k).is_ok_and(starts))
            .filter(|&k| self.chain_reaches(k, c))
            .collect()
    }

    fn chain_reaches(&self, mut k: usize, c: usize) -> bool {
        while let Some(p) = self.parent_of(k) {
            if p == c {
                return true;
            }
            k = p;
        }
        false
    }

    /// Raw (MUTF-8) descriptor of class_def `i`, without decoding it.
    pub fn class_desc_bytes(&self, i: usize) -> R<&'a [u8]> {
        let t = u32_at(self.data, self.class_defs + i * 32)?;
        self.string_bytes(u32_at(self.data, self.type_ids + t as usize * 4)?)
    }

    /// Superclass descriptor of class_def `i`; needs only the id tables and strings.
    pub fn superclass(&self, i: usize) -> R<Option<String>> {
        match u32_at(self.data, self.class_defs + i * 32 + 8)? {
            u32::MAX => Ok(None),
            sup => self.type_name(sup).map(Some),
        }
    }

    pub fn class_def(&self, i: usize) -> R<ClassDef> {
        let off = self.class_defs + i * 32;
        Ok(ClassDef {
            access: u32_at(self.data, off + 4)?,
            class: self.class_descriptor(i)?,
            superclass: self.superclass(i)?,
            interfaces: type_list(self.data, u32_at(self.data, off + 12)? as usize)?
                .into_iter()
                .map(|t| self.type_name(t))
                .collect::<R<_>>()?,
            source_file: match u32_at(self.data, off + 16)? {
                u32::MAX => None,
                s => self.string(s).ok(),
            },
        })
    }

    /// direct then virtual methods of class_def `i`
    pub fn methods(&self, i: usize) -> R<Vec<Method<'a>>> {
        Ok(self.class_data(i)?.methods)
    }

    pub fn class_data(&self, i: usize) -> R<ClassData<'a>> {
        let mut off = u32_at(self.data, self.class_defs + i * 32 + 24)? as usize;
        if off == 0 {
            return Ok(ClassData {
                fields: vec![],
                methods: vec![],
            }); // marker interface etc.
        }
        let d = self.data;
        let sf = uleb128(d, &mut off)?;
        let inf = uleb128(d, &mut off)?;
        let dm = uleb128(d, &mut off)?;
        let vm = uleb128(d, &mut off)?;
        let mut fields = Vec::with_capacity((sf + inf) as usize);
        for count in [sf, inf] {
            let mut idx = 0u32; // field_idx is delta-encoded, restarting per list
            for _ in 0..count {
                idx += uleb128(d, &mut off)?;
                fields.push((idx, uleb128(d, &mut off)?));
            }
        }
        let mut out = Vec::with_capacity((dm + vm) as usize);
        for count in [dm, vm] {
            let mut idx = 0u32; // method_idx is delta-encoded, restarting per list
            for _ in 0..count {
                idx += uleb128(d, &mut off)?;
                let access = uleb128(d, &mut off)?;
                let code_off = uleb128(d, &mut off)? as usize;
                let code = if code_off == 0 {
                    None
                } else {
                    let units = u32_at(d, code_off + 12)? as usize;
                    let insns = d
                        .get(code_off + 16..code_off + 16 + units * 2)
                        .ok_or_else(|| Error::dex("code past end"))?;
                    let tries = parse_tries(d, code_off, units)?;
                    Some(Code {
                        registers: u16_at(d, code_off)?,
                        ins: u16_at(d, code_off + 2)?,
                        insns,
                        tries,
                    })
                };
                out.push(Method {
                    method_idx: idx,
                    access,
                    code,
                });
            }
        }
        Ok(ClassData {
            fields,
            methods: out,
        })
    }
}

impl<'a> Dex<'a> {
    /// Id of the string equal to `s`: string ids are sorted by content, so a binary search.
    /// shortcut: compares MUTF-8 bytes, which orders like the spec's UTF-16 except for
    /// supplementary characters; a name with those may be missed.
    pub fn find_string(&self, s: &[u8]) -> Option<u32> {
        let (mut lo, mut hi) = (0usize, self.counts()[0]);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.string_bytes(mid as u32).ok()?.cmp(s) {
                std::cmp::Ordering::Equal => return Some(mid as u32),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Type id whose descriptor is string `sid` (type ids are sorted by string id).
    pub fn find_type(&self, sid: u32) -> Option<u32> {
        let (mut lo, mut hi) = (0usize, self.counts()[1]);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match u32_at(self.data, self.type_ids + mid * 4).ok()?.cmp(&sid) {
                std::cmp::Ordering::Equal => return Some(mid as u32),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// (owner type id, name string id) of a field (`method = false`) or method id, unallocated.
    pub fn member_ids(&self, method: bool, idx: u32) -> R<(u32, u32)> {
        let off = if method {
            self.method_ids
        } else {
            self.field_ids
        } + idx as usize * 8;
        Ok((u16_at(self.data, off)? as u32, u32_at(self.data, off + 4)?))
    }

    /// (offset of string data, string id) for every string, sorted by offset.
    pub fn string_offsets(&self) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = (0..self.counts()[0] as u32)
            .filter_map(|i| Some((u32_at(self.data, self.string_ids + i as usize * 4).ok()?, i)))
            .collect();
        if !v.windows(2).all(|w| w[0].0 <= w[1].0) {
            v.sort_unstable();
        }
        v
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    /// (method_idx, bytecode or None) of each method of class `i`, in declaration order,
    /// skipping try/catch tables: the cheap walk for listings and xref scans.
    pub fn each_method(&self, i: usize, mut f: impl FnMut(u32, Option<&'a [u8]>)) -> R<()> {
        let mut off = u32_at(self.data, self.class_defs + i * 32 + 24)? as usize;
        if off == 0 {
            return Ok(());
        }
        let d = self.data;
        let (sf, inf, dm, vm) = (
            uleb128(d, &mut off)?,
            uleb128(d, &mut off)?,
            uleb128(d, &mut off)?,
            uleb128(d, &mut off)?,
        );
        for _ in 0..(sf + inf) * 2 {
            uleb128(d, &mut off)?;
        }
        for count in [dm, vm] {
            let mut idx = 0u32;
            for _ in 0..count {
                idx += uleb128(d, &mut off)?;
                uleb128(d, &mut off)?;
                let code_off = uleb128(d, &mut off)? as usize;
                let code = if code_off == 0 {
                    None
                } else {
                    let units = u32_at(d, code_off + 12)? as usize;
                    Some(
                        d.get(code_off + 16..code_off + 16 + units * 2)
                            .ok_or_else(|| Error::dex("code past end"))?,
                    )
                };
                f(idx, code);
            }
        }
        Ok(())
    }

    /// Proto id of a method id.
    pub fn method_proto(&self, idx: u32) -> R<u32> {
        u16_at(self.data, self.method_ids + idx as usize * 8 + 2).map(u32::from)
    }
}

/// A value inside an annotation (`encoded_value`).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// value, kind: b(yte) s(hort) c(har) i(nt) j (long)
    Int(i64, char),
    Float(f32),
    Double(f64),
    Str(u32),
    Type(u32),
    Field(u32),
    Method(u32),
    Enum(u32),
    MethodType(u32),
    MethodHandle(u32),
    Array(Vec<Value>),
    Annotation(Annotation),
    Null,
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    /// 0 build, 1 runtime, 2 system
    pub visibility: u8,
    pub type_idx: u32,
    /// (name string idx, value)
    pub elements: Vec<(u32, Value)>,
}

#[derive(Default)]
pub struct Annotations {
    pub class: Vec<Annotation>,
    pub fields: std::collections::HashMap<u32, Vec<Annotation>>,
    pub methods: std::collections::HashMap<u32, Vec<Annotation>>,
    /// per method: one annotation list per parameter
    pub params: std::collections::HashMap<u32, Vec<Vec<Annotation>>>,
}

fn encoded_value(d: &[u8], off: &mut usize, depth: u32) -> R<Value> {
    if depth > 32 {
        return Err(Error::dex("annotation nesting too deep"));
    }
    let h = *d.get(*off).ok_or_else(|| Error::dex("value past end"))?;
    *off += 1;
    let (ty, arg) = (h & 0x1f, (h >> 5) as usize);
    let mut raw = |n: usize| -> R<u64> {
        let b = d
            .get(*off..*off + n)
            .ok_or_else(|| Error::dex("value past end"))?;
        *off += n;
        Ok(b.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64))
    };
    let signed = |v: u64, n: usize| ((v << (64 - 8 * n)) as i64) >> (64 - 8 * n);
    Ok(match ty {
        0x00 => Value::Int(signed(raw(1)?, 1), 'b'),
        0x02 => Value::Int(signed(raw(arg + 1)?, arg + 1), 's'),
        0x03 => Value::Int(raw(arg + 1)? as i64, 'c'),
        0x04 => Value::Int(signed(raw(arg + 1)?, arg + 1), 'i'),
        0x06 => Value::Int(signed(raw(arg + 1)?, arg + 1), 'j'),
        // floats keep their high bytes; missing low bytes are zero
        0x10 => Value::Float(f32::from_bits(
            (raw(arg + 1)? << (8 * (3 - arg.min(3)))) as u32,
        )),
        0x11 => Value::Double(f64::from_bits(raw(arg + 1)? << (8 * (7 - arg.min(7))))),
        0x15 => Value::MethodType(raw(arg + 1)? as u32),
        0x16 => Value::MethodHandle(raw(arg + 1)? as u32),
        0x17 => Value::Str(raw(arg + 1)? as u32),
        0x18 => Value::Type(raw(arg + 1)? as u32),
        0x19 => Value::Field(raw(arg + 1)? as u32),
        0x1a => Value::Method(raw(arg + 1)? as u32),
        0x1b => Value::Enum(raw(arg + 1)? as u32),
        0x1c => {
            let n = uleb128(d, off)?;
            Value::Array(
                (0..n)
                    .map(|_| encoded_value(d, off, depth + 1))
                    .collect::<R<_>>()?,
            )
        }
        0x1d => Value::Annotation(encoded_annotation(d, off, 3, depth + 1)?),
        0x1e => Value::Null,
        0x1f => Value::Bool(arg != 0),
        t => return Err(Error::dex(format!("bad encoded value type {t:#x}"))),
    })
}

fn encoded_annotation(d: &[u8], off: &mut usize, visibility: u8, depth: u32) -> R<Annotation> {
    let type_idx = uleb128(d, off)?;
    let n = uleb128(d, off)?;
    let mut elements = Vec::with_capacity(n.min(64) as usize);
    for _ in 0..n {
        let name = uleb128(d, off)?;
        elements.push((name, encoded_value(d, off, depth)?));
    }
    Ok(Annotation {
        visibility,
        type_idx,
        elements,
    })
}

fn annotation_set(d: &[u8], off: usize) -> R<Vec<Annotation>> {
    if off == 0 {
        return Ok(vec![]);
    }
    let n = u32_at(d, off)? as usize;
    (0..n)
        .map(|k| {
            let mut o = u32_at(d, off + 4 + k * 4)? as usize;
            let vis = *d.get(o).ok_or_else(|| Error::dex("annotation past end"))?;
            o += 1;
            encoded_annotation(d, &mut o, vis, 0)
        })
        .collect()
}

impl<'a> Dex<'a> {
    /// Initial values of the class's static fields, in static-field order (may be shorter).
    pub fn static_values(&self, c: usize) -> R<Vec<Value>> {
        let mut off = u32_at(self.data, self.class_defs + c * 32 + 28)? as usize;
        if off == 0 {
            return Ok(vec![]);
        }
        let n = uleb128(self.data, &mut off)?;
        (0..n)
            .map(|_| encoded_value(self.data, &mut off, 0))
            .collect()
    }

    pub fn annotations(&self, c: usize) -> R<Annotations> {
        let d = self.data;
        let dir = u32_at(d, self.class_defs + c * 32 + 20)? as usize;
        let mut out = Annotations::default();
        if dir == 0 {
            return Ok(out);
        }
        out.class = annotation_set(d, u32_at(d, dir)? as usize)?;
        let (nf, nm, np) = (
            u32_at(d, dir + 4)? as usize,
            u32_at(d, dir + 8)? as usize,
            u32_at(d, dir + 12)? as usize,
        );
        let mut o = dir + 16;
        for _ in 0..nf {
            out.fields.insert(
                u32_at(d, o)?,
                annotation_set(d, u32_at(d, o + 4)? as usize)?,
            );
            o += 8;
        }
        for _ in 0..nm {
            out.methods.insert(
                u32_at(d, o)?,
                annotation_set(d, u32_at(d, o + 4)? as usize)?,
            );
            o += 8;
        }
        for _ in 0..np {
            let list = u32_at(d, o + 4)? as usize;
            let n = u32_at(d, list)? as usize;
            let sets = (0..n)
                .map(|k| annotation_set(d, u32_at(d, list + 4 + k * 4)? as usize))
                .collect::<R<_>>()?;
            out.params.insert(u32_at(d, o)?, sets);
            o += 8;
        }
        Ok(out)
    }
}

/// `Lcom/foo/Bar;` -> `com.foo.Bar`
/// 64-bit hash of a name, for compact lookup tables.
pub fn name_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// DEX strings are MUTF-8: NUL is `C0 80` and characters above U+FFFF are two encoded
/// surrogates. Plain UTF-8 (nearly every string) takes the fast path.
pub fn mutf8(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    let mut units = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as u16;
        let next = |k: usize| b.get(i + k).map_or(0, |&x| x as u16 & 0x3f);
        let (u, len) = match c {
            0..=0x7f => (c, 1),
            0xc0..=0xdf => ((c & 0x1f) << 6 | next(1), 2),
            0xe0..=0xef => ((c & 0x0f) << 12 | next(1) << 6 | next(2), 3),
            _ => (0xfffd, 1),
        };
        units.push(u);
        i += len;
    }
    String::from_utf16_lossy(&units)
}

/// `com.a.B`, `com/a/B` or `Lcom/a/B;` -> `Lcom/a/B;`
pub fn descriptor(name: &str) -> String {
    if name.starts_with('L') && name.ends_with(';') {
        return name.to_string();
    }
    format!("L{};", name.replace('.', "/"))
}

/// One instruction as text, its references resolved: `invoke-virtual {v0}, La/B;->c()V`.
pub fn render(dex: &Dex, i: &crate::bytecode::Insn) -> Result<String> {
    use crate::bytecode::{self, Fmt::*, Ref};
    let (name, f, r) = bytecode::info(i.op).unwrap();
    let mut parts = Vec::new();
    let regs: Vec<String> = if i.range {
        (0..i.nregs as u16)
            .map(|k| format!("v{}", i.regs[0] + k))
            .collect()
    } else {
        i.regs[..i.nregs as usize]
            .iter()
            .map(|r| format!("v{r}"))
            .collect()
    };
    if matches!(f, F35c | F3rc | F45cc | F4rcc) {
        parts.push(format!("{{{}}}", regs.join(", ")));
    } else {
        parts.extend(regs);
    }
    match f {
        F10t | F20t | F30t | F21t | F22t | F31t => {
            parts.push(format!("{:04x}", i.off as i64 + i.lit))
        }
        F11n | F21s | F21h | F31i | F51l | F22b | F22s => parts.push(format!("#{}", i.lit)),
        _ => {}
    }
    match r {
        Ref::None => {}
        Ref::String => parts.push(format!("{:?}", dex.string(i.idx)?)),
        Ref::Type => parts.push(dex.type_name(i.idx)?),
        Ref::Field => parts.push(dex.field_ref(i.idx)?),
        Ref::Method => parts.push(dex.method_ref(i.idx)?),
        Ref::Proto => parts.push(dex.proto(i.idx)?),
        // shortcut: call sites / method handles shown by index, resolve when lambdas get decompiled
        Ref::CallSite => parts.push(format!("call_site@{}", i.idx)),
        Ref::MethodHandle => parts.push(format!("method_handle@{}", i.idx)),
    }
    if matches!(f, F45cc | F4rcc) {
        parts.push(dex.proto(i.idx2)?);
    }
    Ok(format!("{name} {}", parts.join(", "))
        .trim_end()
        .to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// hand-built DEX: 1 string, 1 type, 1 class_def `a.b.Cls`
    pub(crate) fn tiny_dex() -> Vec<u8> {
        let mut d = vec![0u8; 0x70];
        d[..8].copy_from_slice(b"dex\n035\0");
        let put =
            |d: &mut Vec<u8>, off: usize, v: u32| d[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut d, 0x3C, 0x70); // string_ids
        put(&mut d, 0x44, 0x74); // type_ids
        put(&mut d, 0x60, 1);
        put(&mut d, 0x64, 0x78); // class_defs
        d.resize(0x78 + 32, 0);
        put(&mut d, 0x70, 0x98); // string data offset
        let s = b"\x0aLa/b/Cls;\0";
        d.extend_from_slice(s);
        d
    }

    #[test]
    fn reads_class_name() {
        let d = tiny_dex();
        let dex = Dex::parse(&d).unwrap();
        assert_eq!(dex.class_descriptor(0).unwrap(), "La/b/Cls;");
        assert!(dex.methods(0).unwrap().is_empty());
        assert!(Dex::parse(b"nope").is_err());
    }

    /// a DEX inflated only through its names: the interface list is past the end, but the
    /// superclass (what `refs extends` needs) still reads
    #[test]
    fn superclass_without_data_section() {
        let mut d = tiny_dex();
        d[0x78 + 8..0x78 + 12].copy_from_slice(&0u32.to_le_bytes()); // extends type 0
        d[0x78 + 12..0x78 + 16].copy_from_slice(&0x10000u32.to_le_bytes()); // interfaces, not inflated
        let dex = Dex::parse(&d).unwrap();
        assert_eq!(dex.superclass(0).unwrap().as_deref(), Some("La/b/Cls;"));
        assert!(dex.class_def(0).is_err());
    }

    #[test]
    fn uleb() {
        let mut o = 0;
        assert_eq!(uleb128(&[0xe5, 0x8e, 0x26], &mut o).unwrap(), 624485);
        assert_eq!(o, 3);
        let mut o = 0;
        assert_eq!(sleb128(&[0x7f], &mut o).unwrap(), -1);
        let mut o = 0;
        assert_eq!(sleb128(&[0x80, 0x7f], &mut o).unwrap(), -128);
    }
}
