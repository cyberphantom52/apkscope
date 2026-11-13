//! Text XML -> Android binary XML (the format of a compiled AndroidManifest.xml), so an edited
//! manifest can go back into an APK. Attribute value types follow the original file where the
//! attribute already existed, so `versionName="1"` stays a string.

use std::collections::HashMap;

use crate::res::{Names, framework_id};
use crate::{Error, Result};

const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";

// ---------- a small XML reader (elements, attributes, text) ----------

#[derive(Debug, PartialEq)]
pub enum Node {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        line: u32,
    },
    End {
        name: String,
        line: u32,
    },
    Text(String),
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn parse(text: &str) -> Result<Vec<Node>> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    let line_at = |i: usize| text[..i].matches('\n').count() as u32 + 1;
    let mut stack: Vec<String> = Vec::new();
    while i < b.len() {
        if text[i..].starts_with("<?") {
            i += text[i..]
                .find("?>")
                .ok_or_else(|| Error::xml("unterminated <?"))?
                + 2;
        } else if text[i..].starts_with("<!--") {
            i += text[i..]
                .find("-->")
                .ok_or_else(|| Error::xml("unterminated comment"))?
                + 3;
        } else if text[i..].starts_with("</") {
            let end = i + text[i..]
                .find('>')
                .ok_or_else(|| Error::xml("unterminated end tag"))?;
            let name = text[i + 2..end].trim().to_string();
            match stack.pop() {
                Some(open) if open == name => {}
                Some(open) => {
                    return Err(Error::xml(format!(
                        "line {}: </{name}> closes <{open}>",
                        line_at(i)
                    )));
                }
                None => {
                    return Err(Error::xml(format!(
                        "line {}: </{name}> without an open tag",
                        line_at(i)
                    )));
                }
            }
            out.push(Node::End {
                name,
                line: line_at(i),
            });
            i = end + 1;
        } else if b[i] == b'<' {
            let line = line_at(i);
            let mut j = i + 1;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let name = text[i + 1..j].to_string();
            if name.is_empty() {
                return Err(Error::xml(format!("line {line}: empty tag name")));
            }
            let mut attrs = Vec::new();
            loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                match b.get(j) {
                    None => return Err(Error::xml(format!("line {line}: unterminated <{name}>"))),
                    Some(b'>') => {
                        out.push(Node::Start {
                            name: name.clone(),
                            attrs,
                            line,
                        });
                        stack.push(name);
                        j += 1;
                        break;
                    }
                    Some(b'/') if b.get(j + 1) == Some(&b'>') => {
                        out.push(Node::Start {
                            name: name.clone(),
                            attrs,
                            line,
                        });
                        out.push(Node::End { name, line });
                        j += 2;
                        break;
                    }
                    _ => {
                        let k = j + text[j..].find('=').ok_or_else(|| {
                            Error::xml(format!("line {}: attribute without a value", line_at(j)))
                        })?;
                        let an = text[j..k].trim().to_string();
                        let q = k
                            + 1
                            + text[k + 1..].find(['"', '\'']).ok_or_else(|| {
                                Error::xml(format!(
                                    "line {}: attribute {an} needs quotes",
                                    line_at(k)
                                ))
                            })?;
                        let quote = b[q] as char;
                        let e = q
                            + 1
                            + text[q + 1..].find(quote).ok_or_else(|| {
                                Error::xml(format!(
                                    "line {}: unterminated value of {an}",
                                    line_at(q)
                                ))
                            })?;
                        attrs.push((an, unescape(&text[q + 1..e])));
                        j = e + 1;
                    }
                }
            }
            i = j;
        } else {
            let end = text[i..].find('<').map_or(b.len(), |p| i + p);
            let t = text[i..end].trim();
            if !t.is_empty() {
                out.push(Node::Text(unescape(t)));
            }
            i = end;
        }
    }
    if let Some(open) = stack.pop() {
        return Err(Error::xml(format!("<{open}> is never closed")));
    }
    Ok(out)
}

// ---------- reading the original's attribute types and ids ----------

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// `prefix:name` / `name` -> (value type, resource id) of every attribute in a binary XML file.
pub fn original_attrs(d: &[u8]) -> HashMap<String, (u8, Option<u32>)> {
    let mut out = HashMap::new();
    let Ok(text) = crate::res::decode_xml(d, None) else {
        return out;
    };
    // the decoder gives names with prefixes; walk the binary for the types in the same order
    let names: Vec<String> = match parse(&text) {
        Ok(nodes) => nodes
            .into_iter()
            .filter_map(|n| {
                if let Node::Start { attrs, .. } = n {
                    Some(
                        attrs
                            .into_iter()
                            .map(|a| a.0)
                            .filter(|a| !a.starts_with("xmlns:"))
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                }
            })
            .flatten()
            .collect(),
        Err(_) => return out,
    };
    let mut res_ids = Vec::new();
    let mut types = Vec::new();
    let mut at = u16_at(d, 2).unwrap_or(8) as usize;
    while let (Some(ty), Some(h), Some(size)) =
        (u16_at(d, at), u16_at(d, at + 2), u32_at(d, at + 4))
    {
        let (h, size) = (h as usize, size as usize);
        if size < 8 {
            break;
        }
        match ty {
            0x0180 => {
                res_ids = (0..(size - h) / 4)
                    .filter_map(|k| u32_at(d, at + h + k * 4))
                    .collect()
            }
            0x0102 => {
                let body = at + h;
                let (start, asize, count) = (
                    u16_at(d, body + 8).unwrap_or(20) as usize,
                    u16_at(d, body + 10).unwrap_or(20) as usize,
                    u16_at(d, body + 12).unwrap_or(0) as usize,
                );
                for k in 0..count {
                    let a = body + start + k * asize;
                    let name_idx = u32_at(d, a + 4).unwrap_or(u32::MAX) as usize;
                    types.push((
                        d.get(a + 15).copied().unwrap_or(3),
                        res_ids.get(name_idx).copied(),
                    ));
                }
            }
            _ => {}
        }
        at += size;
    }
    for (n, t) in names.into_iter().zip(types) {
        out.entry(n).or_insert(t);
    }
    out
}

// ---------- typed values ----------

/// Complex (dimension) value: mantissa with the largest radix that still fits 23 bits.
/// Resolve `@type/name`, `@android:type/name`, `@0x7f..`, `?attr/x` to (type, id).
fn reference(s: &str, names: &Names) -> Result<(u8, u32)> {
    let (ty, body) = if let Some(b) = s.strip_prefix('?') {
        (0x02, b)
    } else {
        (0x01, &s[1..])
    };
    if body == "null" {
        return Ok((0x01, 0));
    }
    let (android, body) = match body.strip_prefix("android:") {
        Some(b) => (true, b),
        None => (false, body),
    };
    if let Some(hex) = body.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16)
            .map(|id| (ty, id))
            .map_err(|_| Error::xml(format!("bad resource id {s}")));
    }
    let (t, n) = body
        .split_once('/')
        .ok_or_else(|| Error::xml(format!("{s}: expected @type/name")))?;
    let id = if android {
        framework_id(t, n)
    } else {
        names.id_of(t, n)
    };
    id.map(|id| (ty, id))
        .ok_or_else(|| Error::xml(format!("unknown resource {s}")))
}

/// (raw string, type, data) for an attribute value.
fn typed(text: &str, orig: Option<u8>, names: &Names) -> Result<(Option<String>, u8, u32)> {
    let string = Ok((Some(text.to_string()), 0x03, 0));
    // an attribute that was a string stays one, whatever it looks like now
    if orig == Some(0x03) {
        return string;
    }
    if text.starts_with('@') || text.starts_with('?') {
        let (t, id) = reference(text, names)?;
        return Ok((None, t, id));
    }
    match text {
        "true" => return Ok((None, 0x12, 0xffff_ffff)),
        "false" => return Ok((None, 0x12, 0)),
        _ => {}
    }
    if let Some(hex) = text.strip_prefix("0x")
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Ok((None, 0x11, v));
    }
    if let Ok(v) = text.parse::<i32>() {
        return Ok((None, 0x10, v as u32));
    }
    if let Some(hex) = text.strip_prefix('#') {
        let ty = match hex.len() {
            8 => 0x1c,
            6 => 0x1d,
            4 => 0x1e,
            3 => 0x1f,
            _ => return string,
        };
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return Ok((None, ty, v));
        }
    }
    for (unit, code) in [
        ("px", 0),
        ("dip", 1),
        ("dp", 1),
        ("sp", 2),
        ("pt", 3),
        ("in", 4),
        ("mm", 5),
    ] {
        if let Some(num) = text.strip_suffix(unit)
            && let Ok(v) = num.parse::<f64>()
        {
            return Ok((None, 0x05, crate::res::float_to_complex(v as f32, code)));
        }
    }
    if orig.is_some_and(|t| t != 0x03) {
        return Err(Error::xml(format!(
            "\"{text}\" can't be stored as the type this attribute had"
        )));
    }
    string
}

// ---------- encoding ----------

struct Pool {
    /// (string, attribute resource id): the same text with and without an id are separate
    items: Vec<(String, Option<u32>)>,
    index: HashMap<(String, Option<u32>), u32>,
}

impl Pool {
    fn get(&mut self, s: &str, id: Option<u32>) -> u32 {
        let key = (s.to_string(), id);
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        let i = self.items.len() as u32;
        self.items.push(key.clone());
        self.index.insert(key, i);
        i
    }

    /// UTF-8 string pool chunk.
    fn chunk(&self) -> Vec<u8> {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        let len = |o: &mut Vec<u8>, n: usize| {
            if n > 0x7f {
                o.push(0x80 | (n >> 8) as u8);
            }
            o.push((n & 0xff) as u8);
        };
        for (s, _) in &self.items {
            offsets.push(data.len() as u32);
            len(&mut data, s.encode_utf16().count());
            len(&mut data, s.len());
            data.extend_from_slice(s.as_bytes());
            data.push(0);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let start = 28 + 4 * offsets.len();
        let mut c = Vec::new();
        push16(&mut c, 0x0001);
        push16(&mut c, 28);
        push32(&mut c, (start + data.len()) as u32);
        push32(&mut c, offsets.len() as u32);
        push32(&mut c, 0);
        push32(&mut c, 0x100); // UTF-8
        push32(&mut c, start as u32);
        push32(&mut c, 0);
        offsets.iter().for_each(|o| push32(&mut c, *o));
        c.extend(data);
        c
    }
}

fn push16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}

fn push32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}

/// One node header: type, header size 16, size, line number, comment (none).
fn node_head(o: &mut Vec<u8>, ty: u16, size: u32, line: u32) {
    push16(o, ty);
    push16(o, 16);
    push32(o, size);
    push32(o, line);
    push32(o, u32::MAX);
}

/// Encode XML text as Android binary XML. `original` (the file being replaced) supplies the
/// types and resource ids of attributes that already existed.
pub fn encode(text: &str, original: Option<&[u8]>, names: &Names) -> Result<Vec<u8>> {
    let nodes = parse(text)?;
    let orig = original.map(original_attrs).unwrap_or_default();
    // namespace prefixes, from xmlns: attributes anywhere in the document
    let mut ns: HashMap<String, String> = HashMap::new();
    for n in &nodes {
        if let Node::Start { attrs, .. } = n {
            for (k, v) in attrs {
                if let Some(p) = k.strip_prefix("xmlns:") {
                    ns.insert(p.to_string(), v.clone());
                }
            }
        }
    }
    let attr_id = |full: &str| -> Result<Option<u32>> {
        let Some((prefix, name)) = full.split_once(':') else {
            return Ok(None);
        };
        if let Some(&(_, Some(id))) = orig.get(full) {
            return Ok(Some(id));
        }
        match ns.get(prefix).map(String::as_str) {
            Some(ANDROID_NS) => framework_id("attr", name)
                .map(Some)
                .ok_or_else(|| Error::xml(format!("unknown attribute {full}"))),
            Some(_) => Ok(None),
            None => Err(Error::xml(format!("undeclared namespace prefix in {full}"))),
        }
    };
    // attribute names with resource ids go first, in id order, matching the resource map
    let mut pool = Pool {
        items: Vec::new(),
        index: HashMap::new(),
    };
    let mut with_ids: Vec<(u32, String)> = Vec::new();
    for n in &nodes {
        if let Node::Start { attrs, .. } = n {
            for (k, _) in attrs.iter().filter(|a| !a.0.starts_with("xmlns:")) {
                if let Some(id) = attr_id(k)? {
                    with_ids.push((
                        id,
                        k.split_once(':').map_or(k.as_str(), |x| x.1).to_string(),
                    ));
                }
            }
        }
    }
    with_ids.sort();
    with_ids.dedup();
    for (id, name) in &with_ids {
        pool.get(name, Some(*id));
    }
    let res_map: Vec<u32> = with_ids.iter().map(|x| x.0).collect();
    let mut body = Vec::new();
    let mut open_ns: Vec<Vec<(u32, u32)>> = Vec::new();
    for n in &nodes {
        match n {
            Node::Start { name, attrs, line } => {
                let mut declared = Vec::new();
                for (k, v) in attrs.iter().filter(|a| a.0.starts_with("xmlns:")) {
                    let (p, u) = (pool.get(&k[6..], None), pool.get(v, None));
                    let mut c = Vec::new();
                    node_head(&mut c, 0x0100, 24, *line);
                    push32(&mut c, p);
                    push32(&mut c, u);
                    body.extend(c);
                    declared.push((p, u));
                }
                open_ns.push(declared);
                let mut list = Vec::new();
                for (k, v) in attrs.iter().filter(|a| !a.0.starts_with("xmlns:")) {
                    let id = attr_id(k)?;
                    let (uri, local) = match k.split_once(':') {
                        Some((p, l)) => (Some(ns[p].clone()), l.to_string()),
                        None => (None, k.clone()),
                    };
                    let (raw, ty, data) = typed(v, orig.get(k).map(|x| x.0), names)
                        .map_err(|e| e.at(format!("line {line}, {k}")))?;
                    list.push((id, uri, local, raw, ty, data));
                }
                list.sort_by(|a, b| {
                    (a.0.unwrap_or(u32::MAX), &a.2).cmp(&(b.0.unwrap_or(u32::MAX), &b.2))
                });
                let elem = name.as_str();
                let mut c = Vec::new();
                node_head(&mut c, 0x0102, 36 + 20 * list.len() as u32, *line);
                push32(&mut c, u32::MAX);
                push32(&mut c, pool.get(elem, None));
                push16(&mut c, 20);
                push16(&mut c, 20);
                push16(&mut c, list.len() as u16);
                // 1-based positions of the id/class/style attributes
                let pos = |want: &str| {
                    list.iter()
                        .position(|a| {
                            a.1.is_none() && a.2 == want
                                || a.1.as_deref() == Some(ANDROID_NS) && want == "id" && a.2 == "id"
                        })
                        .map_or(0, |p| p as u16 + 1)
                };
                push16(&mut c, pos("id"));
                push16(&mut c, pos("class"));
                push16(&mut c, pos("style"));
                for (id, uri, local, raw, ty, data) in &list {
                    push32(&mut c, uri.as_ref().map_or(u32::MAX, |u| pool.get(u, None)));
                    push32(&mut c, pool.get(local, *id));
                    let raw_idx = raw.as_ref().map_or(u32::MAX, |r| pool.get(r, None));
                    push32(&mut c, raw_idx);
                    push16(&mut c, 8);
                    c.push(0);
                    c.push(*ty);
                    push32(&mut c, if *ty == 0x03 { raw_idx } else { *data });
                }
                body.extend(c);
            }
            Node::End { name, line } => {
                let mut c = Vec::new();
                node_head(&mut c, 0x0103, 24, *line);
                push32(&mut c, u32::MAX);
                push32(&mut c, pool.get(name, None));
                body.extend(c);
                for (p, u) in open_ns.pop().unwrap_or_default().into_iter().rev() {
                    let mut c = Vec::new();
                    node_head(&mut c, 0x0101, 24, *line);
                    push32(&mut c, p);
                    push32(&mut c, u);
                    body.extend(c);
                }
            }
            Node::Text(t) => {
                let i = pool.get(t, None);
                let mut c = Vec::new();
                node_head(&mut c, 0x0104, 28, 0);
                push32(&mut c, i);
                push16(&mut c, 8);
                c.push(0);
                c.push(0x03);
                push32(&mut c, i);
                body.extend(c);
            }
        }
    }
    let pool_chunk = pool.chunk();
    let mut map = Vec::new();
    push16(&mut map, 0x0180);
    push16(&mut map, 8);
    push32(&mut map, 8 + 4 * res_map.len() as u32);
    res_map.iter().for_each(|id| push32(&mut map, *id));
    let mut out = Vec::new();
    push16(&mut out, 0x0003);
    push16(&mut out, 8);
    push32(
        &mut out,
        (8 + pool_chunk.len() + map.len() + body.len()) as u32,
    );
    out.extend(pool_chunk);
    out.extend(map);
    out.extend(body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reports_errors() {
        let n = parse("<?xml version=\"1.0\"?>\n<a x=\"1 &amp; 2\">\n  <!-- c -->\n  <b/>hi</a>")
            .unwrap();
        assert_eq!(
            n[0],
            Node::Start {
                name: "a".into(),
                attrs: vec![("x".into(), "1 & 2".into())],
                line: 2
            }
        );
        assert_eq!(
            n[1],
            Node::Start {
                name: "b".into(),
                attrs: vec![],
                line: 4
            }
        );
        assert_eq!(n[3], Node::Text("hi".into()));
        assert!(
            parse("<a><b></a>")
                .unwrap_err()
                .to_string()
                .contains("closes <b>")
        );
        assert!(
            parse("<a x=1/>")
                .unwrap_err()
                .to_string()
                .contains("needs quotes")
        );
    }

    #[test]
    fn types_and_complex() {
        let names = crate::res::Names::new(&crate::table::Table {
            strings: crate::table::StringPool {
                utf8: true,
                sorted: false,
                strings: vec![],
                styles: vec![],
            },
            packages: vec![],
        });
        assert_eq!(
            typed("true", None, &names).unwrap(),
            (None, 0x12, 0xffff_ffff)
        );
        assert_eq!(typed("21", None, &names).unwrap(), (None, 0x10, 21));
        // was a string in the original: stays one
        assert_eq!(typed("21", Some(0x03), &names).unwrap().1, 0x03);
        assert_eq!(
            typed("@android:style/Theme", None, &names).unwrap(),
            (None, 0x01, 0x0103_0005)
        );
        assert!(typed("@string/missing", None, &names).is_err());
        // 16dp: mantissa 16 at radix 23p0... decodes back to 16
        let c = crate::res::float_to_complex(16.0, 1);
        assert_eq!(c & 0xf, 1);
        let mant =
            (c as i32 >> 8) as f64 / f64::from(1u32 << [0, 7, 15, 23][((c >> 4) & 3) as usize]);
        assert!((mant - 16.0).abs() < 1e-6);
    }

    /// decode -> encode -> decode gives the same text, on the local test APKs when present
    #[test]
    fn real_manifests_round_trip() {
        for apk in ["D:/APKS/Zestle_v0.6.8-prealpha.apk", "D:/APKS/Unciv.apk"] {
            if !std::path::Path::new(apk).exists() {
                continue;
            }
            let mut a = crate::apk::open(apk).unwrap();
            let table = crate::table::read(&a.read("resources.arsc").unwrap()).unwrap();
            let names = crate::res::Names::new(&table);
            let bin = a.read("AndroidManifest.xml").unwrap();
            let text = crate::res::decode_xml(&bin, Some(&names)).unwrap();
            let again = encode(&text, Some(&bin), &names).unwrap();
            let text2 = crate::res::decode_xml(&again, Some(&names)).unwrap();
            assert_eq!(text, text2, "{apk}");
        }
    }
}
