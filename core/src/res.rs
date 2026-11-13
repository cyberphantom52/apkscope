//! Android resources as text: a [`Table`]'s values as `res/values*/*.xml`, and binary XML
//! (AXML) documents. Format: frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h

use crate::table::{Chunk, EntryValue, Package, Span, StringPool, Table, Value, config};
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

type R<T> = Result<T>;

fn u8_at(d: &[u8], o: usize) -> R<u8> {
    d.get(o)
        .copied()
        .ok_or_else(|| Error::resources(format!("read past end at {o:#x}")))
}
fn u16_at(d: &[u8], o: usize) -> R<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| Error::resources(format!("read past end at {o:#x}")))
}
fn u32_at(d: &[u8], o: usize) -> R<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| Error::resources(format!("read past end at {o:#x}")))
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

/// A package's resources by ID: type and name. An obfuscated table can give several resources
/// of a type one name; each of those is named after its ID too (`name_0x7f030001`), so that
/// every name says which resource it is.
pub fn resource_names(p: &Package) -> BTreeMap<u32, (String, String)> {
    let mut names = BTreeMap::new();
    for c in &p.chunks {
        let Chunk::Type(ty) = c else { continue };
        for (i, e) in ty.entries.iter().enumerate() {
            let Some(e) = e else { continue };
            let id = p.id << 24 | (ty.id as u32) << 16 | i as u32;
            names.entry(id).or_insert_with(|| {
                let tn = p
                    .type_name(ty.id)
                    .map_or_else(|| format!("type{}", ty.id), str::to_owned);
                let kn = p
                    .keys
                    .strings
                    .get(e.key as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("res_{id:08x}"));
                (tn, kn)
            });
        }
    }
    let mut count: HashMap<(&str, &str), usize> = HashMap::new();
    for (t, n) in names.values() {
        *count.entry((t, n)).or_default() += 1;
    }
    let shared: Vec<u32> = names
        .iter()
        .filter(|(_, (t, n))| count[&(t.as_str(), n.as_str())] > 1)
        .map(|(id, _)| *id)
        .collect();
    for id in shared {
        let (_, n) = names.get_mut(&id).expect("listed above");
        *n = format!("{n}_0x{id:08x}");
    }
    names
}

/// Resource id -> `type/name`.
pub struct Names {
    map: HashMap<u32, (String, String)>,
    /// The package whose resources a reference names without a package.
    app: u32,
}

impl Names {
    /// The names of every package's resources.
    pub fn new(t: &Table) -> Names {
        let mut map = HashMap::new();
        for p in &t.packages {
            for (id, name) in resource_names(p) {
                map.entry(id).or_insert(name);
            }
        }
        Names {
            map,
            app: t.app().map_or(0x7f, |p| p.id),
        }
    }

    /// `@string/app_name`, or a hex id for framework / unknown resources
    pub fn reference(&self, id: u32, sigil: char) -> String {
        match self.map.get(&id) {
            // an app's table can carry the framework's package too
            Some((t, n)) if id >> 24 == 0x01 && self.app != 0x01 => {
                format!("{sigil}android:{t}/{n}")
            }
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

/// A dimension or fraction as stored: a 24-bit mantissa in one of four fixed-point radixes,
/// and a unit. This is aapt2's encoding (`ResTable::stringToFloat`), so the same text gives the
/// same bits: the radix is the most precise one the value fits.
pub(crate) fn float_to_complex(f: f32, unit: u32) -> u32 {
    let neg = f < 0.0;
    let bits = (f.abs() * (1u32 << 23) as f32) as f64 + 0.5;
    let bits = bits as u64;
    let (radix, shift) = if bits & 0x7f_ffff == 0 {
        (0, 23)
    } else if bits & 0xffff_ffff_ff80_0000 == 0 {
        (3, 0)
    } else if bits & 0xffff_ffff_8000_0000 == 0 {
        (2, 8)
    } else if bits & 0xffff_ff80_0000_0000 == 0 {
        (1, 16)
    } else {
        (0, 23)
    };
    let mut mantissa = ((bits >> shift) & 0xff_ffff) as u32;
    if neg {
        mantissa = mantissa.wrapping_neg() & 0xff_ffff;
    }
    mantissa << 8 | radix << 4 | unit
}

/// The value of a dimension or fraction.
fn complex_value(data: u32) -> f64 {
    const SHIFT: [u32; 4] = [0, 7, 15, 23];
    let mantissa = (data as i32 >> 8) as f64;
    mantissa / (1u32 << SHIFT[((data >> 4) & 3) as usize]) as f64
}

/// A dimension or fraction as text: the shortest number that encodes back to `data`.
/// `scale` is what the number is multiplied by when it is stored, 1/100 for a percentage.
fn complex(data: u32, units: &[&str], scale: f32) -> String {
    let unit = units.get((data & 0xf) as usize).copied().unwrap_or("");
    let v = complex_value(data) / scale as f64;
    let shortest = (0..=10).find_map(|digits| {
        let s = trim_number(format!("{v:.digits$}"));
        let f: f32 = s.parse().ok()?;
        (float_to_complex(f * scale, data & 0xf) == data).then_some(s)
    });
    format!("{}{unit}", shortest.unwrap_or_else(|| v.to_string()))
}

fn trim_number(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    let t = s.trim_end_matches('0').trim_end_matches('.');
    match t {
        "" | "-" | "-0" => "0".into(),
        t => t.into(),
    }
}

/// The units of a dimension and a fraction, by their code.
pub(crate) const DIMENSION_UNITS: [&str; 6] = ["px", "dp", "sp", "pt", "in", "mm"];
pub(crate) const FRACTION_UNITS: [&str; 2] = ["%", "%p"];

/// Text form of a typed value as it would appear in resource XML.
pub fn format_value(v: Value, strings: &[String], names: &Names) -> String {
    let Value { ty, data } = v;
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
        // the shortest text that reads back as the same float
        0x04 => f32::from_bits(data).to_string(),
        0x05 => complex(data, &DIMENSION_UNITS, 1.0),
        // fractions are stored as 0..1
        0x06 => complex(data, &FRACTION_UNITS, 0.01),
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

/// A string value as the body of a resource element, as aapt2 reads it back: `\\`, `\"`,
/// `\'`, `\n` and `\t` escaped, a leading `@` or `?` escaped, the whole in quotes when its
/// whitespace would otherwise be collapsed, and each of its style spans as a tag around the
/// characters it covers (`<b>`, `<font size="12">`, ...).
fn string_xml(pool: &StringPool, i: u32) -> String {
    let text = pool.strings.get(i as usize).map_or("", String::as_str);
    let spans = pool.styles.get(i as usize).map_or(&[][..], Vec::as_slice);
    string_body(text, spans, &pool.strings)
}

fn string_body(text: &str, spans: &[Span], strings: &[String]) -> String {
    // whitespace that aapt2 would collapse: `\n` and `\t` are written escaped, and keep
    let collapses = |c: char| c.is_whitespace() && c != '\n' && c != '\t';
    let quote = text.starts_with(collapses)
        || text.ends_with(collapses)
        || text.contains("  ")
        || text.contains(|c: char| collapses(c) && c != ' ');
    let tag = |s: &Span, open: bool| -> String {
        let name = strings.get(s.name as usize).map_or("span", String::as_str);
        let mut parts = name.split(';');
        let tag = parts.next().unwrap_or("span");
        if !open {
            return format!("</{tag}>");
        }
        let attrs: String = parts
            .filter_map(|a| a.split_once('='))
            .map(|(k, v)| format!(" {k}=\"{}\"", xml_escape(v)))
            .collect();
        format!("<{tag}{attrs}>")
    };
    // spans by where they open, the longest first; they close innermost first
    let mut spans: Vec<&Span> = spans.iter().collect();
    spans.sort_by_key(|s| (s.first, std::cmp::Reverse(s.last)));
    let mut next = spans.into_iter().peekable();
    let mut open: Vec<&Span> = Vec::new();
    let mut out = String::with_capacity(text.len() + 2);
    if quote {
        out.push('"');
    }
    let mut unit = 0u32; // position in UTF-16 units, as spans count
    let mut chars = text.chars();
    loop {
        while let Some(s) = open.last().filter(|s| s.last.wrapping_add(1) <= unit) {
            out.push_str(&tag(s, false));
            open.pop();
        }
        while let Some(s) = next.next_if(|s| s.first <= unit) {
            if s.last.wrapping_add(1) == s.first {
                // a span over no characters: `<br/>`
                out.push_str(&tag(s, true).replacen('>', "/>", 1));
            } else {
                out.push_str(&tag(s, true));
                open.push(s);
            }
        }
        let Some(c) = chars.next() else { break };
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '@' | '?' if unit == 0 => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
        unit += c.len_utf16() as u32;
    }
    for s in open.iter().rev() {
        out.push_str(&tag(s, false));
    }
    if quote {
        out.push('"');
    }
    out
}

/// Decode a binary XML document to text. `names` resolves @references when available.
pub fn decode_xml(d: &[u8], names: Option<&Names>) -> R<String> {
    if u16_at(d, 0)? != 0x0003 {
        return Err(Error::resources("not binary XML"));
    }
    let empty = Names {
        map: HashMap::new(),
        app: 0x7f,
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
            0x0001 => strings = crate::table::read::pool(d, at)?.strings,
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
                    let value = Value {
                        ty: u8_at(d, a + 15)?,
                        data: u32_at(d, a + 16)?,
                    };
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
fn framework_constant(attr: &str, Value { ty, data }: Value) -> Option<String> {
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

/// A configuration's entries by index: key, value.
type Entries<'a> = BTreeMap<usize, (u32, &'a EntryValue)>;

/// The app package's values, as `values*/` files, and its file-backed resources.
pub fn decode_table(t: &Table, names: &Names) -> Decoded {
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut files = Vec::new();
    let Some(p) = t.app() else {
        return Decoded {
            values: BTreeMap::new(),
            files,
        };
    };
    let strings = &t.strings.strings;
    let resources = resource_names(p);
    // by type and qualifiers, then by entry: one directory per configuration
    let mut configs: BTreeMap<(u8, String), Entries> = BTreeMap::new();
    for c in &p.chunks {
        let Chunk::Type(ty) = c else { continue };
        let qual = match config::name(&ty.config) {
            q if q.is_empty() => q,
            q => format!("-{q}"),
        };
        let map = configs.entry((ty.id, qual)).or_default();
        for (i, e) in ty.entries.iter().enumerate() {
            if let Some(e) = e {
                map.insert(i, (e.key, &e.value));
            }
        }
    }
    for ((ty, qual), entries) in &configs {
        let tname = p
            .type_name(*ty)
            .map_or_else(|| format!("type{ty}"), str::to_owned);
        for (index, (_, entry)) in entries {
            let id = p.id << 24 | (*ty as u32) << 16 | *index as u32;
            let name = xml_escape(resources.get(&id).map_or("unnamed", |(_, n)| n.as_str()));
            let fmt = |v: Value| format_value(v, strings, names);
            // a string with its spans; anything else, a reference say, as itself
            let text = |v: Value| match v.ty {
                Value::STRING => string_xml(&t.strings, v.data),
                _ => xml_escape(&fmt(v)),
            };
            match entry {
                // a string value pointing into res/ is a file resource
                EntryValue::Simple(Value {
                    ty: Value::STRING,
                    data: i,
                }) if strings
                    .get(*i as usize)
                    .is_some_and(|p| p.starts_with("res/"))
                    && tname != "string" =>
                {
                    let src = strings[*i as usize].clone();
                    let ext = if src.ends_with(".9.png") {
                        ".9.png"
                    } else {
                        src.rfind('.').map_or("", |k| &src[k..])
                    };
                    files.push((format!("{tname}{qual}/{name}{ext}"), src));
                }
                EntryValue::Simple(v) => {
                    let line = match tname.as_str() {
                        "string" => format!("    <string name=\"{name}\">{}</string>", text(*v)),
                        "id" => format!("    <item type=\"id\" name=\"{name}\" />"),
                        "color" | "dimen" | "bool" | "integer" | "fraction" => {
                            format!("    <{tname} name=\"{name}\">{}</{tname}>", text(*v))
                        }
                        _ => format!(
                            "    <item type=\"{tname}\" name=\"{name}\">{}</item>",
                            text(*v)
                        ),
                    };
                    values
                        .entry(format!("values{qual}/{}.xml", file_for(&tname)))
                        .or_default()
                        .push(line);
                }
                EntryValue::Complex { parent, map: items } => {
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
                            } else if name.contains('.') {
                                // without it, a dotted name implies a parent: `A.B` is under `A`
                                " parent=\"\"".into()
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
                                    text(*v)
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
                                    text(*v)
                                );
                            }
                            s += "    </plurals>";
                        }
                        "array" => {
                            let tag = if items.iter().all(|(_, v)| v.ty == Value::STRING) {
                                "string-array"
                            } else if items.iter().all(|(_, v)| v.ty == Value::INT_DEC) {
                                "integer-array"
                            } else {
                                "array"
                            };
                            let _ = writeln!(s, "    <{tag} name=\"{name}\">");
                            for (_, v) in items {
                                let _ = writeln!(s, "        <item>{}</item>", text(*v));
                            }
                            let _ = write!(s, "    </{tag}>");
                        }
                        "attr" => {
                            let format = items
                                .iter()
                                .find(|(k, _)| *k == 0x01000000)
                                .map_or(0xffff, |(_, v)| v.data);
                            let kids: Vec<_> = items
                                // the format, min, max and quantity keys, not the framework's
                                // own enum names
                                .iter()
                                .filter(|(k, _)| k & 0xffff_0000 != 0x0100_0000)
                                .collect();
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
                                    text(*v)
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
    // every resource's ID, so that a rebuild keeps them where compiled code expects them
    let mut public: BTreeMap<u32, String> = BTreeMap::new();
    for ((ty, _), entries) in &configs {
        let tname = p
            .type_name(*ty)
            .map_or_else(|| format!("type{ty}"), str::to_owned);
        for index in entries.keys() {
            let id = p.id << 24 | (*ty as u32) << 16 | *index as u32;
            public.entry(id).or_insert_with(|| {
                let name = resources.get(&id).map_or("unnamed", |(_, n)| n.as_str());
                format!(
                    "    <public type=\"{}\" name=\"{}\" id=\"0x{id:08x}\" />",
                    xml_escape(&tname),
                    xml_escape(name)
                )
            });
        }
    }
    if !public.is_empty() {
        values.insert("values/public.xml".into(), public.into_values().collect());
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
            app: 0x7f,
        };
        assert_eq!(
            format_value(Value { ty: 0x12, data: 1 }, &[], &names),
            "true"
        );
        assert_eq!(
            format_value(
                Value {
                    ty: 0x1c,
                    data: 0xff112233
                },
                &[],
                &names
            ),
            "#ff112233"
        );
        // 16dp: mantissa 16, radix 0, unit dp
        assert_eq!(
            format_value(
                Value {
                    ty: 0x05,
                    data: (16 << 8) | 1
                },
                &[],
                &names
            ),
            "16dp"
        );
        // 1.5sp: mantissa 1.5 * 2^7 = 192, radix 1
        assert_eq!(
            format_value(
                Value {
                    ty: 0x05,
                    data: (192 << 8) | (1 << 4) | 2
                },
                &[],
                &names
            ),
            "1.5sp"
        );
        let body = |s: &str| string_body(s, &[], &[]);
        assert_eq!(body("it's @home"), "it\\'s @home");
        assert_eq!(body("@x"), "\\@x");
        assert_eq!(body("say \"hi\"\n"), "say \\\"hi\\\"\\n");
        assert_eq!(body(" two  spaces"), "\" two  spaces\"");
    }

    #[test]
    fn framework_names() {
        let names = Names {
            map: HashMap::new(),
            app: 0x7f,
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
            framework_constant(
                "layout_width",
                Value {
                    ty: 0x10,
                    data: 0xfffffffe
                }
            )
            .as_deref(),
            Some("wrap_content")
        );
    }

    #[test]
    fn utf8_pool() {
        // pool header (28 bytes) + 1 offset + "hi"
        let mut d = vec![0u8; 28];
        d[0..2].copy_from_slice(&1u16.to_le_bytes());
        d[2..4].copy_from_slice(&28u16.to_le_bytes());
        d[4..8].copy_from_slice(&40u32.to_le_bytes());
        d[8..12].copy_from_slice(&1u32.to_le_bytes());
        d[16..20].copy_from_slice(&0x100u32.to_le_bytes());
        d[20..24].copy_from_slice(&32u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&[2, 2, b'h', b'i', 0, 0, 0, 0]);
        assert_eq!(crate::table::read::pool(&d, 0).unwrap().strings, ["hi"]);
    }
}

#[cfg(test)]
mod corpus {
    use super::*;
    use crate::table::{self, Chunk, EntryValue, Value};

    /// Every dimension, fraction and float of every table in a directory prints as text that
    /// encodes back to the same bits:
    /// APKSCOPE_TABLES=<dir> cargo test --release numbers_round_trip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn numbers_round_trip() {
        let dir = std::env::var("APKSCOPE_TABLES").expect("set APKSCOPE_TABLES");
        let names = Names {
            map: HashMap::new(),
            app: 0x7f,
        };
        let (mut checked, mut wrong) = (0, 0);
        for e in std::fs::read_dir(dir).unwrap() {
            let Ok(t) = table::read(&std::fs::read(e.unwrap().path()).unwrap()) else {
                continue;
            };
            let mut values = vec![];
            for p in &t.packages {
                for c in &p.chunks {
                    let Chunk::Type(ty) = c else { continue };
                    for e in ty.entries.iter().flatten() {
                        match &e.value {
                            EntryValue::Simple(v) => values.push(*v),
                            EntryValue::Complex { map, .. } => {
                                values.extend(map.iter().map(|x| x.1))
                            }
                        }
                    }
                }
            }
            for v in values {
                let text = format_value(v, &[], &names);
                let back = match v.ty {
                    Value::FLOAT => text.parse::<f32>().map(f32::to_bits).ok(),
                    Value::DIMENSION | Value::FRACTION => {
                        let (units, scale): (&[&str], f32) = if v.ty == Value::DIMENSION {
                            (&DIMENSION_UNITS, 1.0)
                        } else {
                            (&FRACTION_UNITS, 0.01)
                        };
                        let unit = (v.data & 0xf) as usize;
                        let num = text.trim_end_matches(units.get(unit).copied().unwrap_or(""));
                        num.parse::<f32>()
                            .ok()
                            .map(|f| float_to_complex(f * scale, unit as u32))
                    }
                    _ => continue,
                };
                checked += 1;
                if back != Some(v.data) {
                    wrong += 1;
                    if wrong <= 10 {
                        println!("{:#x} {:#010x} -> {text} -> {back:x?}", v.ty, v.data);
                    }
                }
            }
        }
        println!("{checked} numbers, {wrong} don't encode back");
        assert_eq!(wrong, 0);
    }
}
