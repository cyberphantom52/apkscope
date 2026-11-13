//! [`Table`] -> `resources.arsc` bytes, laid out as aapt2 lays them out.

use super::*;

const RES_STRING_POOL: u16 = 0x0001;
const RES_TABLE: u16 = 0x0002;
const RES_TABLE_PACKAGE: u16 = 0x0200;
const RES_TABLE_TYPE: u16 = 0x0201;
const RES_TABLE_TYPE_SPEC: u16 = 0x0202;

fn u16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn pad4(o: &mut Vec<u8>) {
    while !o.len().is_multiple_of(4) {
        o.push(0);
    }
}

/// A chunk of `ty` with a header of `header` bytes, `body` writing everything after the
/// type and sizes; its size is filled in when it is done.
fn chunk(o: &mut Vec<u8>, ty: u16, header: u16, body: impl FnOnce(&mut Vec<u8>)) {
    let start = o.len();
    u16(o, ty);
    u16(o, header);
    u32(o, 0);
    body(o);
    pad4(o);
    let size = (o.len() - start) as u32;
    o[start + 4..start + 8].copy_from_slice(&size.to_le_bytes());
}

pub fn write(t: &Table) -> Vec<u8> {
    let mut o = Vec::new();
    chunk(&mut o, RES_TABLE, 12, |o| {
        u32(o, t.packages.len() as u32);
        pool(o, &t.strings);
        for p in &t.packages {
            package(o, p);
        }
    });
    o
}

/// A length of one or two units, the first with the high bit set when there are two.
fn length(o: &mut Vec<u8>, n: usize, wide: bool) {
    match (wide, n) {
        (false, 0..0x80) => o.push(n as u8),
        (false, _) => o.extend_from_slice(&[0x80 | (n >> 8) as u8, n as u8]),
        (true, 0..0x8000) => u16(o, n as u16),
        (true, _) => {
            u16(o, 0x8000 | (n >> 16) as u16);
            u16(o, n as u16);
        }
    }
}

/// UTF-8 as Android writes it: a character outside the BMP as a surrogate pair, each half
/// encoded on its own in three bytes.
fn cesu8(s: &str) -> Vec<u8> {
    if s.chars().all(|c| (c as u32) < 0x10000) {
        return s.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(s.len() + 2);
    for unit in s.encode_utf16() {
        match unit {
            0..0x80 => out.push(unit as u8),
            0x80..0x800 => {
                out.extend_from_slice(&[0xc0 | (unit >> 6) as u8, 0x80 | (unit & 0x3f) as u8])
            }
            _ => out.extend_from_slice(&[
                0xe0 | (unit >> 12) as u8,
                0x80 | ((unit >> 6) & 0x3f) as u8,
                0x80 | (unit & 0x3f) as u8,
            ]),
        }
    }
    out
}

fn pool(o: &mut Vec<u8>, p: &StringPool) {
    let mut data = Vec::new();
    let mut offsets = Vec::with_capacity(p.strings.len());
    for s in &p.strings {
        offsets.push(data.len() as u32);
        if p.utf8 {
            length(&mut data, s.encode_utf16().count(), false);
            let bytes = cesu8(s);
            length(&mut data, bytes.len(), false);
            data.extend_from_slice(&bytes);
            data.push(0);
        } else {
            let units: Vec<u16> = s.encode_utf16().collect();
            length(&mut data, units.len(), true);
            units.iter().for_each(|&c| u16(&mut data, c));
            u16(&mut data, 0);
        }
    }
    pad4(&mut data);
    let mut styles = Vec::new();
    let mut style_offsets = Vec::with_capacity(p.styles.len());
    for spans in &p.styles {
        style_offsets.push(styles.len() as u32);
        for s in spans {
            u32(&mut styles, s.name);
            u32(&mut styles, s.first);
            u32(&mut styles, s.last);
        }
        u32(&mut styles, u32::MAX);
    }
    if !p.styles.is_empty() {
        // the style data ends with two more END markers
        u32(&mut styles, u32::MAX);
        u32(&mut styles, u32::MAX);
    }
    let header = 28u32;
    let strings_start = header + 4 * (offsets.len() + style_offsets.len()) as u32;
    chunk(o, RES_STRING_POOL, header as u16, |o| {
        u32(o, p.strings.len() as u32);
        u32(o, p.styles.len() as u32);
        u32(o, if p.utf8 { 0x100 } else { 0 } | p.sorted as u32);
        u32(o, strings_start);
        u32(
            o,
            if p.styles.is_empty() {
                0
            } else {
                strings_start + data.len() as u32
            },
        );
        offsets
            .iter()
            .chain(&style_offsets)
            .for_each(|&v| u32(o, v));
        o.extend_from_slice(&data);
        o.extend_from_slice(&styles);
    });
}

fn package(o: &mut Vec<u8>, p: &Package) {
    let mut types = Vec::new();
    pool(&mut types, &p.types);
    chunk(o, RES_TABLE_PACKAGE, 288, |o| {
        u32(o, p.id);
        let mut name: Vec<u16> = p.name.encode_utf16().collect();
        name.resize(128, 0);
        name.iter().for_each(|&c| u16(o, c));
        u32(o, 288);
        u32(o, p.last_public_type);
        u32(o, 288 + types.len() as u32);
        u32(o, p.last_public_key);
        u32(o, p.type_id_offset);
        o.extend_from_slice(&types);
        pool(o, &p.keys);
        for c in &p.chunks {
            match c {
                Chunk::Spec(s) => spec(o, s),
                Chunk::Type(t) => type_chunk(o, t),
                Chunk::Other(raw) => o.extend_from_slice(raw),
            }
        }
    });
}

fn spec(o: &mut Vec<u8>, s: &Spec) {
    chunk(o, RES_TABLE_TYPE_SPEC, 16, |o| {
        o.push(s.id);
        o.push(s.res0);
        u16(o, s.res1);
        u32(o, s.flags.len() as u32);
        s.flags.iter().for_each(|&f| u32(o, f));
    });
}

fn type_chunk(o: &mut Vec<u8>, t: &Type) {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    let present = t.entries.iter().filter(|e| e.is_some()).count();
    for (index, e) in t.entries.iter().enumerate() {
        let Some(e) = e else {
            if t.flags & Type::SPARSE == 0 {
                if t.flags & Type::OFFSET16 != 0 {
                    u16(&mut offsets, 0xffff);
                } else {
                    u32(&mut offsets, u32::MAX);
                }
            }
            continue;
        };
        let at = data.len();
        if t.flags & Type::SPARSE != 0 {
            u16(&mut offsets, index as u16);
            u16(&mut offsets, (at / 4) as u16);
        } else if t.flags & Type::OFFSET16 != 0 {
            u16(&mut offsets, (at / 4) as u16);
        } else {
            u32(&mut offsets, at as u32);
        }
        entry(&mut data, e);
    }
    pad4(&mut offsets);
    let header = 20 + t.config.len();
    let count = if t.flags & Type::SPARSE != 0 {
        present
    } else {
        t.entries.len()
    };
    chunk(o, RES_TABLE_TYPE, header as u16, |o| {
        o.push(t.id);
        o.push(t.flags);
        u16(o, t.reserved);
        u32(o, count as u32);
        u32(o, (header + offsets.len()) as u32);
        o.extend_from_slice(&t.config);
        o.extend_from_slice(&offsets);
        o.extend_from_slice(&data);
    });
}

fn entry(o: &mut Vec<u8>, e: &Entry) {
    match &e.value {
        EntryValue::Simple(v) if e.flags & Entry::COMPACT != 0 => {
            u16(o, e.key as u16);
            u16(o, e.flags | (v.ty as u16) << 8);
            u32(o, v.data);
        }
        EntryValue::Simple(v) => {
            u16(o, 8);
            u16(o, e.flags);
            u32(o, e.key);
            value(o, *v);
        }
        EntryValue::Complex { parent, map } => {
            u16(o, 16);
            u16(o, e.flags);
            u32(o, e.key);
            u32(o, *parent);
            u32(o, map.len() as u32);
            for (name, v) in map {
                u32(o, *name);
                value(o, *v);
            }
        }
    }
}

fn value(o: &mut Vec<u8>, v: Value) {
    u16(o, 8);
    o.push(0);
    o.push(v.ty);
    u32(o, v.data);
}
