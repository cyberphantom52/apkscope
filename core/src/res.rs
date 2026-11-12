//! Android resources: `resources.arsc` table and binary XML (AXML) decoding.
//! Format: frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

type R<T> = Result<T, String>;

fn u8_at(d: &[u8], o: usize) -> R<u8> {
    d.get(o)
        .copied()
        .ok_or_else(|| format!("read past end at {o:#x}"))
}
fn u16_at(d: &[u8], o: usize) -> R<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("read past end at {o:#x}"))
}
fn u32_at(d: &[u8], o: usize) -> R<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("read past end at {o:#x}"))
}

/// ResStringPool -> all strings (style spans are ignored)
pub(crate) fn string_pool(d: &[u8], at: usize) -> R<Vec<String>> {
    let count = u32_at(d, at + 8)? as usize;
    let utf8 = u32_at(d, at + 16)? & 0x100 != 0;
    let data = at + u32_at(d, at + 20)? as usize;
    let offsets = at + u16_at(d, at + 2)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let mut o = data + u32_at(d, offsets + i * 4)? as usize;
        let s = if utf8 {
            // utf16 length then utf8 length, each 1 or 2 bytes
            let len8 = |o: &mut usize| -> R<usize> {
                let b = u8_at(d, *o)? as usize;
                *o += 1;
                if b & 0x80 == 0 {
                    return Ok(b);
                }
                let lo = u8_at(d, *o)? as usize;
                *o += 1;
                Ok(((b & 0x7f) << 8) | lo)
            };
            len8(&mut o)?;
            let len = len8(&mut o)?;
            String::from_utf8_lossy(d.get(o..o + len).ok_or("string past end")?).into_owned()
        } else {
            let mut len = u16_at(d, o)? as usize;
            o += 2;
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | u16_at(d, o)? as usize;
                o += 2;
            }
            let units: Vec<u16> = (0..len).map(|k| u16_at(d, o + k * 2)).collect::<R<_>>()?;
            String::from_utf16_lossy(&units)
        };
        out.push(s);
    }
    Ok(out)
}

/// A typed value (Res_value): type, data
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Value(pub u8, pub u32);

pub enum Entry {
    Simple(Value),
    /// parent, (attribute/key id, value)
    Bag(u32, Vec<(u32, Value)>),
}

/// One resource table package (usually the app, id 0x7f).
pub struct Table {
    pub package: String,
    pub id: u8,
    pub values: Vec<String>,
    pub types: Vec<String>,
    pub keys: Vec<String>,
    /// (type id, config qualifier) -> entries (entry id -> (key index, entry))
    pub configs: BTreeMap<(u8, String), BTreeMap<u16, (u32, Entry)>>,
}

pub fn parse_table(d: &[u8]) -> R<Table> {
    if u16_at(d, 0)? != 0x0002 {
        return Err("not a resource table".into());
    }
    let mut t = Table {
        package: String::new(),
        id: 0x7f,
        values: vec![],
        types: vec![],
        keys: vec![],
        configs: BTreeMap::new(),
    };
    let mut at = u16_at(d, 2)? as usize;
    let mut pkgs = vec![];
    while at + 8 <= d.len() {
        let (ty, size) = (u16_at(d, at)?, u32_at(d, at + 4)? as usize);
        if size < 8 {
            break;
        }
        match ty {
            0x0001 => t.values = string_pool(d, at)?,
            0x0200 => pkgs.push((u32_at(d, at + 8)?, at)),
            _ => {}
        }
        at += size;
    }
    // shortcut: one package is decoded, the app's own (0x7f) when there are several (some apps
    // put a library package first), else the first
    if let Some(&(_, at)) = pkgs.iter().find(|p| p.0 == 0x7f).or(pkgs.first()) {
        package(d, at, &mut t)?;
    }
    Ok(t)
}

fn package(d: &[u8], at: usize, t: &mut Table) -> R<()> {
    t.id = u32_at(d, at + 8)? as u8;
    let name: Vec<u16> = (0..128)
        .map(|k| u16_at(d, at + 12 + k * 2))
        .collect::<R<_>>()?;
    t.package = String::from_utf16_lossy(&name)
        .trim_end_matches('\0')
        .to_string();
    t.types = string_pool(d, at + u32_at(d, at + 268)? as usize)?;
    t.keys = string_pool(d, at + u32_at(d, at + 276)? as usize)?;
    let end = at + u32_at(d, at + 4)? as usize;
    let mut c = at + u16_at(d, at + 2)? as usize;
    while c + 8 <= end.min(d.len()) {
        let (ty, size) = (u16_at(d, c)?, u32_at(d, c + 4)? as usize);
        if size < 8 {
            break;
        }
        if ty == 0x0201 {
            type_chunk(d, c, t)?;
        }
        c += size;
    }
    Ok(())
}

fn type_chunk(d: &[u8], c: usize, t: &mut Table) -> R<()> {
    let id = u8_at(d, c + 8)?;
    let flags = u8_at(d, c + 9)?;
    let count = u32_at(d, c + 12)? as usize;
    let entries = c + u32_at(d, c + 16)? as usize;
    let qual = qualifier(d, c + 20)?;
    let offsets = c + u16_at(d, c + 2)? as usize;
    let mut list: Vec<(u16, usize)> = Vec::new();
    if flags & 0x01 != 0 {
        // sparse: (entry index, offset / 4)
        for k in 0..count {
            list.push((
                u16_at(d, offsets + k * 4)?,
                u16_at(d, offsets + k * 4 + 2)? as usize * 4,
            ));
        }
    } else if flags & 0x02 != 0 {
        for k in 0..count {
            let o = u16_at(d, offsets + k * 2)?;
            if o != 0xffff {
                list.push((k as u16, o as usize * 4));
            }
        }
    } else {
        for k in 0..count {
            let o = u32_at(d, offsets + k * 4)?;
            if o != u32::MAX {
                list.push((k as u16, o as usize));
            }
        }
    }
    let map = t.configs.entry((id, qual)).or_default();
    for (k, o) in list {
        let e = entries + o;
        let eflags = u16_at(d, e + 2)?;
        let (key, entry) = if eflags & 0x08 != 0 {
            // compact entry: key in the size slot, value type in the flags' high byte
            (
                u16_at(d, e)? as u32,
                Entry::Simple(Value((eflags >> 8) as u8, u32_at(d, e + 4)?)),
            )
        } else if eflags & 0x01 != 0 {
            let n = u32_at(d, e + 12)? as usize;
            let mut items = Vec::with_capacity(n);
            let base = e + u16_at(d, e)? as usize;
            for i in 0..n {
                let it = base + i * 12;
                items.push((u32_at(d, it)?, Value(u8_at(d, it + 7)?, u32_at(d, it + 8)?)));
            }
            (u32_at(d, e + 4)?, Entry::Bag(u32_at(d, e + 8)?, items))
        } else {
            let v = e + u16_at(d, e)? as usize;
            (
                u32_at(d, e + 4)?,
                Entry::Simple(Value(u8_at(d, v + 3)?, u32_at(d, v + 4)?)),
            )
        };
        map.insert(k, (key, entry));
    }
    Ok(())
}

/// ResTable_config -> `-en-rUS-night-xhdpi-v21` style suffix (common qualifiers only)
pub(crate) fn qualifier(d: &[u8], c: usize) -> R<String> {
    let size = u32_at(d, c)? as usize;
    let get8 = |o: usize| if o < size { u8_at(d, c + o) } else { Ok(0) };
    let get16 = |o: usize| {
        if o + 1 < size {
            u16_at(d, c + o)
        } else {
            Ok(0)
        }
    };
    let mut q = Vec::new();
    let (mcc, mnc) = (get16(4)?, get16(6)?);
    if mcc != 0 {
        q.push(format!("mcc{mcc}"))
    }
    if mnc != 0 {
        q.push(format!("mnc{mnc:02}"))
    }
    let lang = [get8(8)?, get8(9)?];
    if lang[0] != 0 && lang[0] < 0x80 {
        q.push(String::from_utf8_lossy(&lang).into_owned());
        let reg = [get8(10)?, get8(11)?];
        if reg[0] != 0 && reg[0] < 0x80 {
            q.push(format!("r{}", String::from_utf8_lossy(&reg)));
        }
    }
    let layout = get8(28)?;
    match layout & 0xc0 {
        0x40 => q.push("ldltr".into()),
        0x80 => q.push("ldrtl".into()),
        _ => {}
    }
    let (sw, w, h) = (get16(30)?, get16(32)?, get16(34)?);
    if sw != 0 {
        q.push(format!("sw{sw}dp"))
    }
    if w != 0 {
        q.push(format!("w{w}dp"))
    }
    if h != 0 {
        q.push(format!("h{h}dp"))
    }
    match layout & 0x0f {
        1 => q.push("small".into()),
        2 => q.push("normal".into()),
        3 => q.push("large".into()),
        4 => q.push("xlarge".into()),
        _ => {}
    }
    match layout & 0x30 {
        0x10 => q.push("notlong".into()),
        0x20 => q.push("long".into()),
        _ => {}
    }
    match get8(12)? {
        1 => q.push("port".into()),
        2 => q.push("land".into()),
        _ => {}
    }
    let ui = get8(29)?;
    match ui & 0x0f {
        2 => q.push("desk".into()),
        3 => q.push("car".into()),
        4 => q.push("television".into()),
        5 => q.push("appliance".into()),
        6 => q.push("watch".into()),
        7 => q.push("vrheadset".into()),
        _ => {}
    }
    match ui & 0x30 {
        0x10 => q.push("notnight".into()),
        0x20 => q.push("night".into()),
        _ => {}
    }
    match get16(14)? {
        0 => {}
        120 => q.push("ldpi".into()),
        160 => q.push("mdpi".into()),
        213 => q.push("tvdpi".into()),
        240 => q.push("hdpi".into()),
        320 => q.push("xhdpi".into()),
        480 => q.push("xxhdpi".into()),
        640 => q.push("xxxhdpi".into()),
        0xfffe => q.push("anydpi".into()),
        0xffff => q.push("nodpi".into()),
        x => q.push(format!("{x}dpi")),
    }
    let sdk = get16(24)?;
    if sdk != 0 {
        q.push(format!("v{sdk}"))
    }
    Ok(q.iter().map(|s| format!("-{s}")).collect())
}

/// Android's public framework resources (`0x01010054` -> attr `windowBackground`), from AOSP.
fn framework(id: u32) -> Option<(&'static str, &'static str)> {
    static TABLE: std::sync::OnceLock<HashMap<u32, (&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            include_str!("data/android_public.txt")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| {
                    let mut p = l.split(' ');
                    Some((
                        u32::from_str_radix(p.next()?, 16).ok()?,
                        (p.next()?, p.next()?),
                    ))
                })
                .collect()
        })
        .get(&id)
        .copied()
}

/// The reverse: (`attr`, `windowBackground`) -> `0x01010054`.
pub fn framework_id(ty: &str, name: &str) -> Option<u32> {
    static TABLE: std::sync::OnceLock<HashMap<(&'static str, &'static str), u32>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            include_str!("data/android_public.txt")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| {
                    let mut p = l.split(' ');
                    let id = u32::from_str_radix(p.next()?, 16).ok()?;
                    Some(((p.next()?, p.next()?), id))
                })
                .collect()
        })
        .get(&(ty, name))
        .copied()
}

/// Resource id -> `type/name` for the app package.
pub struct Names {
    map: HashMap<u32, (String, String)>,
}

impl Names {
    /// Names straight from `resources.arsc`, without decoding values, configs or the global
    /// string pool: for the manifest, which only names a handful of ids.
    pub fn from_arsc(d: &[u8]) -> R<Names> {
        if u16_at(d, 0)? != 0x0002 {
            return Err("not a resource table".into());
        }
        let mut map = HashMap::new();
        let mut at = u16_at(d, 2)? as usize;
        while at + 8 <= d.len() {
            let (ty, size) = (u16_at(d, at)?, u32_at(d, at + 4)? as usize);
            if size < 8 {
                break;
            }
            if ty == 0x0200 {
                // every package: some apps put a library package (0x7b...) before their own 0x7f
                let pkg = u32_at(d, at + 8)? as u8;
                let types = string_pool(d, at + u32_at(d, at + 268)? as usize)?;
                let keys = string_pool(d, at + u32_at(d, at + 276)? as usize)?;
                let end = (at + size).min(d.len());
                // per type: which entry ids are named yet, and how many; a type whose ids are all
                // named skips its other configs (one per language in a big app)
                let mut seen: HashMap<u8, (Vec<bool>, usize)> = HashMap::new();
                let mut c = at + u16_at(d, at + 2)? as usize;
                while c + 8 <= end {
                    let (cty, csize) = (u16_at(d, c)?, u32_at(d, c + 4)? as usize);
                    if csize < 8 {
                        break;
                    }
                    if cty == 0x0201 {
                        let (id, flags, count) = (
                            u8_at(d, c + 8)?,
                            u8_at(d, c + 9)?,
                            u32_at(d, c + 12)? as usize,
                        );
                        let (entries, offsets) = (
                            c + u32_at(d, c + 16)? as usize,
                            c + u16_at(d, c + 2)? as usize,
                        );
                        let (got, left) = seen.entry(id).or_insert_with(|| (Vec::new(), 0));
                        if count > got.len() {
                            *left += count - got.len();
                            got.resize(count, false);
                        }
                        if *left == 0 {
                            c += csize;
                            continue;
                        }
                        let tn = types
                            .get(id as usize - 1)
                            .cloned()
                            .unwrap_or_else(|| format!("type{id}"));
                        for k in 0..count {
                            // same three offset layouts as `type_chunk`
                            let (e, o) = if flags & 0x01 != 0 {
                                (
                                    u16_at(d, offsets + k * 4)?,
                                    u16_at(d, offsets + k * 4 + 2)? as usize * 4,
                                )
                            } else if flags & 0x02 != 0 {
                                match u16_at(d, offsets + k * 2)? {
                                    0xffff => continue,
                                    o => (k as u16, o as usize * 4),
                                }
                            } else {
                                match u32_at(d, offsets + k * 4)? {
                                    u32::MAX => continue,
                                    o => (k as u16, o as usize),
                                }
                            };
                            let rid = (pkg as u32) << 24 | (id as u32) << 16 | e as u32;
                            match got.get_mut(e as usize) {
                                Some(g) if !*g => {
                                    *g = true;
                                    *left -= 1;
                                }
                                Some(_) => continue,
                                // a sparse index past `count`: rare, named once through the map
                                None if map.contains_key(&rid) => continue,
                                None => {}
                            }
                            let p = entries + o;
                            let key = if u16_at(d, p + 2)? & 0x08 != 0 {
                                u16_at(d, p)? as u32
                            } else {
                                u32_at(d, p + 4)?
                            };
                            let kn = keys
                                .get(key as usize)
                                .cloned()
                                .unwrap_or_else(|| format!("res_{rid:08x}"));
                            map.insert(rid, (tn.clone(), kn));
                        }
                    }
                    c += csize;
                }
            }
            at += size;
        }
        Ok(Names { map })
    }

    pub fn new(t: &Table) -> Names {
        let mut map = HashMap::new();
        for ((ty, _), entries) in &t.configs {
            for (&e, (key, _)) in entries {
                let id = (t.id as u32) << 24 | (*ty as u32) << 16 | e as u32;
                let tn = t
                    .types
                    .get(*ty as usize - 1)
                    .cloned()
                    .unwrap_or_else(|| format!("type{ty}"));
                let kn = t
                    .keys
                    .get(*key as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("res_{id:08x}"));
                map.entry(id).or_insert((tn, kn));
            }
        }
        Names { map }
    }

    /// `@string/app_name`, or a hex id for framework / unknown resources
    pub fn reference(&self, id: u32, sigil: char) -> String {
        match self.map.get(&id) {
            Some((t, n)) => format!("{sigil}{t}/{n}"),
            None if id >> 24 == 0x01 => match framework(id) {
                Some((t, n)) => format!("{sigil}android:{t}/{n}"),
                // non-public or newer than the bundled table
                None => format!("{sigil}android:0x{id:08x}"),
            },
            None if id == 0 => "@null".into(),
            None => format!("{sigil}0x{id:08x}"),
        }
    }

    /// `type/name` -> id, for the app's own resources.
    pub fn id_of(&self, ty: &str, name: &str) -> Option<u32> {
        self.map
            .iter()
            .find(|(_, v)| v.0 == ty && v.1 == name)
            .map(|(k, _)| *k)
    }

    pub fn name(&self, id: u32) -> Option<&str> {
        self.map
            .get(&id)
            .map(|x| x.1.as_str())
            .or_else(|| framework(id).map(|x| x.1))
    }
}

fn complex(data: u32, units: &[&str]) -> String {
    const SHIFT: [u32; 4] = [0, 7, 15, 23];
    let mantissa = (data as i32 >> 8) as f64;
    let v = mantissa / (1u32 << SHIFT[((data >> 4) & 3) as usize]) as f64;
    let unit = units.get((data & 0xf) as usize).copied().unwrap_or("");
    format!("{}{unit}", trim_float(v))
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".into()
    } else {
        s.to_string()
    }
}

/// Text form of a typed value as it would appear in resource XML.
pub fn format_value(v: Value, strings: &[String], names: &Names) -> String {
    let Value(ty, data) = v;
    match ty {
        0x00 => {
            if data == 1 {
                "@empty".into()
            } else {
                "@null".into()
            }
        }
        0x01 | 0x07 => names.reference(data, '@'),
        0x02 | 0x08 => names.reference(data, '?'),
        0x03 => strings.get(data as usize).cloned().unwrap_or_default(),
        0x04 => trim_float(f32::from_bits(data) as f64),
        0x05 => complex(data, &["px", "dp", "sp", "pt", "in", "mm"]),
        0x06 => {
            let s = complex(data, &["%", "%p"]);
            // fractions are stored as 0..1
            let (num, unit) = s.split_at(s.find('%').unwrap_or(s.len()));
            format!(
                "{}{unit}",
                trim_float(num.parse::<f64>().unwrap_or(0.0) * 100.0)
            )
        }
        0x10 => (data as i32).to_string(),
        0x11 => format!("0x{data:08x}"),
        0x12 => (if data != 0 { "true" } else { "false" }).into(),
        0x1c => format!("#{data:08x}"),
        0x1d => format!("#{:06x}", data & 0xffffff),
        0x1e => format!("#{:04x}", data & 0xffff),
        0x1f => format!("#{:03x}", data & 0xfff),
        _ => format!("0x{data:08x}"),
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Escape for a resource string body (apostrophes and leading @/? have meaning there).
fn res_string(s: &str) -> String {
    let mut e = xml_escape(s).replace('\'', "\\'").replace('\n', "\\n");
    if e.starts_with('@') || e.starts_with('?') {
        e.insert(0, '\\');
    }
    e
}

/// Decode a binary XML document to text. `names` resolves @references when available.
pub fn decode_xml(d: &[u8], names: Option<&Names>) -> R<String> {
    if u16_at(d, 0)? != 0x0003 {
        return Err("not binary XML".into());
    }
    let empty = Names {
        map: HashMap::new(),
    };
    let names = names.unwrap_or(&empty);
    let mut strings: Vec<String> = Vec::new();
    let mut res_ids: Vec<u32> = Vec::new();
    let mut ns: HashMap<String, String> = HashMap::new(); // uri -> prefix
    let mut pending_ns: Vec<(String, String)> = Vec::new();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    let mut depth = 0usize;
    let mut open_tag = false; // last start tag not yet closed with '>'
    let s = |strings: &Vec<String>, i: u32| strings.get(i as usize).cloned().unwrap_or_default();
    let mut at = u16_at(d, 2)? as usize;
    while at + 8 <= d.len() {
        let (ty, hsize, size) = (
            u16_at(d, at)?,
            u16_at(d, at + 2)? as usize,
            u32_at(d, at + 4)? as usize,
        );
        if size < 8 {
            break;
        }
        let body = at + hsize;
        match ty {
            0x0001 => strings = string_pool(d, at)?,
            0x0180 => {
                res_ids = (0..(size - hsize) / 4)
                    .map(|k| u32_at(d, body + k * 4))
                    .collect::<R<_>>()?
            }
            0x0100 => {
                let (prefix, uri) = (
                    s(&strings, u32_at(d, body)?),
                    s(&strings, u32_at(d, body + 4)?),
                );
                ns.insert(uri.clone(), prefix.clone());
                pending_ns.push((prefix, uri));
            }
            0x0102 => {
                if open_tag {
                    out += ">\n";
                }
                let name = s(&strings, u32_at(d, body + 4)?);
                let (astart, asize, acount) = (
                    u16_at(d, body + 8)? as usize,
                    u16_at(d, body + 10)? as usize,
                    u16_at(d, body + 12)? as usize,
                );
                let pad = "    ".repeat(depth);
                let _ = write!(out, "{pad}<{name}");
                let attr_pad = format!("\n{pad}    ");
                for (prefix, uri) in pending_ns.drain(..) {
                    let _ = write!(out, "{attr_pad}xmlns:{prefix}=\"{}\"", xml_escape(&uri));
                }
                for k in 0..acount {
                    let a = body + astart + k * asize;
                    let (ans, aname, raw) = (u32_at(d, a)?, u32_at(d, a + 4)?, u32_at(d, a + 8)?);
                    let value = Value(u8_at(d, a + 15)?, u32_at(d, a + 16)?);
                    let mut an = s(&strings, aname);
                    if an.is_empty() {
                        // obfuscated attribute names: fall back to the resource id
                        an = res_ids
                            .get(aname as usize)
                            .map_or(format!("attr{aname}"), |id| {
                                names
                                    .name(*id)
                                    .map_or(format!("attr_{id:08x}"), str::to_string)
                            });
                    }
                    let mut prefix = if ans == u32::MAX {
                        None
                    } else {
                        ns.get(&s(&strings, ans)).cloned()
                    };
                    // Android reads framework attributes by id alone; packers blank or fake their
                    // name and namespace strings, so the id decides both
                    if let Some(("attr", fw)) =
                        res_ids.get(aname as usize).and_then(|&id| framework(id))
                    {
                        an = fw.to_string();
                        prefix = Some(
                            ns.get("http://schemas.android.com/apk/res/android")
                                .cloned()
                                .unwrap_or_else(|| "android".into()),
                        );
                    }
                    let full = prefix.map_or(an.clone(), |p| format!("{p}:{an}"));
                    let text = if raw != u32::MAX {
                        s(&strings, raw)
                    } else {
                        framework_constant(&an, value)
                            .unwrap_or_else(|| format_value(value, &strings, names))
                    };
                    let _ = write!(out, "{attr_pad}{full}=\"{}\"", xml_escape(&text));
                }
                open_tag = true;
                depth += 1;
            }
            0x0103 => {
                depth = depth.saturating_sub(1);
                if open_tag {
                    out += " />\n";
                    open_tag = false;
                } else {
                    let name = s(&strings, u32_at(d, body + 4)?);
                    let _ = writeln!(out, "{}</{name}>", "    ".repeat(depth));
                }
            }
            0x0104 => {
                if open_tag {
                    out += ">\n";
                    open_tag = false;
                }
                let text = s(&strings, u32_at(d, body)?);
                let _ = writeln!(out, "{}{}", "    ".repeat(depth), xml_escape(text.trim()));
            }
            _ => {}
        }
        at += size;
    }
    Ok(out)
}

/// Named constants of common framework attributes (`-2` -> `wrap_content`, `0x11` -> `center`).
fn framework_constant(attr: &str, Value(ty, data): Value) -> Option<String> {
    if !(0x10..=0x11).contains(&ty) {
        return None;
    }
    let enums: &[(u32, &str)] = match attr {
        "layout_width" | "layout_height" => {
            &[(0xffffffff, "match_parent"), (0xfffffffe, "wrap_content")]
        }
        "orientation" => &[(0, "horizontal"), (1, "vertical")],
        "visibility" => &[(0, "visible"), (1, "invisible"), (2, "gone")],
        "textStyle" => &[
            (0, "normal"),
            (1, "bold"),
            (2, "italic"),
            (3, "bold|italic"),
        ],
        "scaleType" => &[
            (0, "matrix"),
            (1, "fitXY"),
            (2, "fitStart"),
            (3, "fitCenter"),
            (4, "fitEnd"),
            (5, "center"),
            (6, "centerCrop"),
            (7, "centerInside"),
        ],
        "ellipsize" => &[(1, "start"), (2, "middle"), (3, "end"), (4, "marquee")],
        "gravity" | "layout_gravity" | "foregroundGravity" => return Some(gravity(data)),
        _ => return None,
    };
    enums.iter().find(|e| e.0 == data).map(|e| e.1.to_string())
}

fn gravity(g: u32) -> String {
    // vertical bits 0x70, horizontal bits 0x07, 0x00800000 = relative (start/end)
    let (v, h, rel) = (g & 0x70, g & 0x07, g & 0x0080_0000 != 0);
    let mut out = Vec::new();
    match (v, h, rel) {
        (0x10, 0x01, false) => out.push("center"),
        (0x70, 0x07, false) => out.push("fill"),
        _ => {
            match h {
                0x03 => out.push(if rel { "start" } else { "left" }),
                0x05 => out.push(if rel { "end" } else { "right" }),
                0x01 => out.push("center_horizontal"),
                0x07 => out.push("fill_horizontal"),
                _ => {}
            }
            match v {
                0x30 => out.push("top"),
                0x50 => out.push("bottom"),
                0x10 => out.push("center_vertical"),
                0x70 => out.push("fill_vertical"),
                _ => {}
            }
        }
    }
    if g & 0x80 != 0 {
        out.push("clip_vertical")
    }
    if g & 0x08 != 0 {
        out.push("clip_horizontal")
    }
    if out.is_empty() {
        format!("0x{g:x}")
    } else {
        out.join("|")
    }
}

/// `values*/` XML files and file-backed resources of a table.
pub struct Decoded {
    /// relative path under res/ -> XML text
    pub values: BTreeMap<String, String>,
    /// (relative path under res/, source path inside the APK)
    pub files: Vec<(String, String)>,
}

const PLURALS: [(u32, &str); 6] = [
    (0x01000004, "other"),
    (0x01000005, "zero"),
    (0x01000006, "one"),
    (0x01000007, "two"),
    (0x01000008, "few"),
    (0x01000009, "many"),
];

pub fn decode_table(t: &Table, names: &Names) -> Decoded {
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut files = Vec::new();
    for ((ty, qual), entries) in &t.configs {
        let tname = t
            .types
            .get(*ty as usize - 1)
            .cloned()
            .unwrap_or_else(|| format!("type{ty}"));
        for (key, entry) in entries.values() {
            let name = xml_escape(t.keys.get(*key as usize).map_or("unnamed", |s| s.as_str()));
            let fmt = |v: Value| format_value(v, &t.values, names);
            match entry {
                // a string value pointing into res/ is a file resource
                Entry::Simple(v @ Value(0x03, i))
                    if t.values
                        .get(*i as usize)
                        .is_some_and(|p| p.starts_with("res/"))
                        && tname != "string" =>
                {
                    let src = t.values[*i as usize].clone();
                    let ext = if src.ends_with(".9.png") {
                        ".9.png"
                    } else {
                        src.rfind('.').map_or("", |k| &src[k..])
                    };
                    let _ = v;
                    files.push((format!("{tname}{qual}/{name}{ext}"), src));
                }
                Entry::Simple(v) => {
                    let line = match tname.as_str() {
                        "string" => format!(
                            "    <string name=\"{name}\">{}</string>",
                            res_string(&fmt(*v))
                        ),
                        "id" => format!("    <item type=\"id\" name=\"{name}\" />"),
                        "color" | "dimen" | "bool" | "integer" | "fraction" => format!(
                            "    <{tname} name=\"{name}\">{}</{tname}>",
                            xml_escape(&fmt(*v))
                        ),
                        _ => format!(
                            "    <item type=\"{tname}\" name=\"{name}\">{}</item>",
                            xml_escape(&fmt(*v))
                        ),
                    };
                    values
                        .entry(format!("values{qual}/{}.xml", file_for(&tname)))
                        .or_default()
                        .push(line);
                }
                Entry::Bag(parent, items) => {
                    let mut s = String::new();
                    match tname.as_str() {
                        "style" => {
                            let p = if *parent != 0 {
                                format!(
                                    " parent=\"{}\"",
                                    xml_escape(
                                        names.reference(*parent, '@').trim_start_matches("@style/")
                                    )
                                )
                            } else {
                                String::new()
                            };
                            let _ = writeln!(s, "    <style name=\"{name}\"{p}>");
                            for (attr, v) in items {
                                let an = names.reference(*attr, '?');
                                // style items name the attribute bare: `colorPrimary`, `android:windowBackground`
                                let an = an.replacen("?android:attr/", "android:", 1);
                                let an = an.trim_start_matches("?attr/").trim_start_matches('?');
                                let _ = writeln!(
                                    s,
                                    "        <item name=\"{}\">{}</item>",
                                    xml_escape(an),
                                    xml_escape(&fmt(*v))
                                );
                            }
                            s += "    </style>";
                        }
                        "plurals" => {
                            let _ = writeln!(s, "    <plurals name=\"{name}\">");
                            for (q, v) in items {
                                let qn =
                                    PLURALS.iter().find(|p| p.0 == *q).map_or("other", |p| p.1);
                                let _ = writeln!(
                                    s,
                                    "        <item quantity=\"{qn}\">{}</item>",
                                    res_string(&fmt(*v))
                                );
                            }
                            s += "    </plurals>";
                        }
                        "array" => {
                            let tag = if items.iter().all(|(_, v)| v.0 == 0x03) {
                                "string-array"
                            } else if items.iter().all(|(_, v)| v.0 == 0x10) {
                                "integer-array"
                            } else {
                                "array"
                            };
                            let _ = writeln!(s, "    <{tag} name=\"{name}\">");
                            for (_, v) in items {
                                let _ = writeln!(
                                    s,
                                    "        <item>{}</item>",
                                    if v.0 == 0x03 {
                                        res_string(&fmt(*v))
                                    } else {
                                        xml_escape(&fmt(*v))
                                    }
                                );
                            }
                            let _ = write!(s, "    </{tag}>");
                        }
                        "attr" => {
                            let format = items
                                .iter()
                                .find(|(k, _)| *k == 0x01000000)
                                .map_or(0xffff, |(_, v)| v.1);
                            let kids: Vec<_> =
                                items.iter().filter(|(k, _)| k >> 24 != 0x01).collect();
                            let kind = if format & 0x20000 != 0 {
                                "flag"
                            } else {
                                "enum"
                            };
                            if kids.is_empty() {
                                let _ = write!(
                                    s,
                                    "    <attr name=\"{name}\" format=\"{}\" />",
                                    attr_format(format)
                                );
                            } else {
                                let _ = writeln!(s, "    <attr name=\"{name}\">");
                                for (k, v) in kids {
                                    let kn = names.name(*k).unwrap_or("unknown");
                                    let _ = writeln!(
                                        s,
                                        "        <{kind} name=\"{}\" value=\"{}\" />",
                                        xml_escape(kn),
                                        fmt(*v)
                                    );
                                }
                                s += "    </attr>";
                            }
                        }
                        _ => {
                            let _ = writeln!(s, "    <item type=\"{tname}\" name=\"{name}\">");
                            for (k, v) in items {
                                let _ = writeln!(
                                    s,
                                    "        <item name=\"{}\">{}</item>",
                                    xml_escape(&names.reference(*k, '@')),
                                    xml_escape(&fmt(*v))
                                );
                            }
                            s += "    </item>";
                        }
                    }
                    values
                        .entry(format!("values{qual}/{}.xml", file_for(&tname)))
                        .or_default()
                        .push(s);
                }
            }
        }
    }
    let values = values
        .into_iter()
        .map(|(path, lines)| {
            (
                path,
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<resources>\n{}\n</resources>\n",
                    lines.join("\n")
                ),
            )
        })
        .collect();
    Decoded { values, files }
}

fn file_for(tname: &str) -> String {
    match tname {
        "string" | "color" | "dimen" | "bool" | "integer" | "style" | "attr" | "fraction"
        | "array" | "id" => format!("{tname}s"),
        "plurals" => "plurals".into(),
        t => format!("{t}s"),
    }
}

fn attr_format(f: u32) -> String {
    let names = [
        (1, "reference"),
        (2, "string"),
        (4, "integer"),
        (8, "boolean"),
        (0x10, "color"),
        (0x20, "float"),
        (0x40, "dimension"),
        (0x80, "fraction"),
    ];
    if f & 0xffff == 0xffff {
        return "reference|string|integer|boolean|color|float|dimension|fraction".into();
    }
    let v: Vec<&str> = names
        .iter()
        .filter(|(b, _)| f & b != 0)
        .map(|x| x.1)
        .collect();
    if v.is_empty() {
        "string".into()
    } else {
        v.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_format() {
        let names = Names {
            map: HashMap::new(),
        };
        assert_eq!(format_value(Value(0x12, 1), &[], &names), "true");
        assert_eq!(
            format_value(Value(0x1c, 0xff112233), &[], &names),
            "#ff112233"
        );
        // 16dp: mantissa 16, radix 0, unit dp
        assert_eq!(
            format_value(Value(0x05, (16 << 8) | 1), &[], &names),
            "16dp"
        );
        // 1.5sp: mantissa 1.5 * 2^7 = 192, radix 1
        assert_eq!(
            format_value(Value(0x05, (192 << 8) | (1 << 4) | 2), &[], &names),
            "1.5sp"
        );
        assert_eq!(res_string("it's @home"), "it\\'s @home");
        assert_eq!(res_string("@x"), "\\@x");
    }

    #[test]
    fn framework_names() {
        let names = Names {
            map: HashMap::new(),
        };
        assert_eq!(
            names.reference(0x01010054, '?'),
            "?android:attr/windowBackground"
        );
        assert_eq!(
            names.reference(0x0106000d, '@'),
            "@android:color/transparent"
        );
        assert_eq!(names.name(0x01010000), Some("theme"));
    }

    #[test]
    fn framework_constants() {
        assert_eq!(gravity(0x11), "center");
        assert_eq!(gravity(0x00800003 | 0x10), "start|center_vertical");
        assert_eq!(
            framework_constant("layout_width", Value(0x10, 0xfffffffe)).as_deref(),
            Some("wrap_content")
        );
    }

    #[test]
    fn utf8_pool() {
        // pool header (28 bytes) + 1 offset + "hi"
        let mut d = vec![0u8; 28];
        d[0..2].copy_from_slice(&1u16.to_le_bytes());
        d[2..4].copy_from_slice(&28u16.to_le_bytes());
        d[8..12].copy_from_slice(&1u32.to_le_bytes());
        d[16..20].copy_from_slice(&0x100u32.to_le_bytes());
        d[20..24].copy_from_slice(&32u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&[2, 2, b'h', b'i', 0]);
        assert_eq!(string_pool(&d, 0).unwrap(), vec!["hi".to_string()]);
    }
}
