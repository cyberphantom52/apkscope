//! `resources.arsc` bytes -> [`Table`].

use super::*;
use crate::{Error, Result};

const RES_STRING_POOL: u16 = 0x0001;
const RES_TABLE: u16 = 0x0002;
const RES_TABLE_PACKAGE: u16 = 0x0200;
const RES_TABLE_TYPE: u16 = 0x0201;
const RES_TABLE_TYPE_SPEC: u16 = 0x0202;

fn past_end(o: usize) -> Error {
    Error::resources(format!("read past end at {o:#x}"))
}
fn u8_at(d: &[u8], o: usize) -> Result<u8> {
    d.get(o).copied().ok_or_else(|| past_end(o))
}
fn u16_at(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| past_end(o))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| past_end(o))
}

/// A chunk's type, header size and size, at `at`.
fn chunk(d: &[u8], at: usize) -> Result<(u16, usize, usize)> {
    let size = u32_at(d, at + 4)? as usize;
    if size < 8 || at + size > d.len() {
        return Err(Error::resources(format!("bad chunk size at {at:#x}")));
    }
    Ok((u16_at(d, at)?, u16_at(d, at + 2)? as usize, size))
}

pub fn read(d: &[u8]) -> Result<Table> {
    let (ty, header, size) = chunk(d, 0)?;
    if ty != RES_TABLE {
        return Err(Error::resources("not a resource table"));
    }
    let mut strings = None;
    let mut packages = Vec::new();
    let mut at = header;
    while at < size {
        let (ty, _, len) = chunk(d, at)?;
        match ty {
            RES_STRING_POOL => strings = Some(pool(d, at)?),
            RES_TABLE_PACKAGE => packages.push(package(d, at)?),
            other => {
                return Err(Error::resources(format!(
                    "unknown chunk {other:#06x} at {at:#x}"
                )));
            }
        }
        at += len;
    }
    Ok(Table {
        strings: strings.ok_or_else(|| Error::resources("no global string pool"))?,
        packages,
    })
}

/// The string pool chunk at `at`: in a table, or in binary XML.
pub(crate) fn pool(d: &[u8], at: usize) -> Result<StringPool> {
    let count = u32_at(d, at + 8)? as usize;
    let style_count = u32_at(d, at + 12)? as usize;
    let flags = u32_at(d, at + 16)?;
    let utf8 = flags & 0x100 != 0;
    let data = at + u32_at(d, at + 20)? as usize;
    let style_data = at + u32_at(d, at + 24)? as usize;
    let offsets = at + u16_at(d, at + 2)? as usize;
    let mut strings = Vec::with_capacity(count);
    for i in 0..count {
        let o = data + u32_at(d, offsets + i * 4)? as usize;
        strings.push(if utf8 {
            utf8_string(d, o)?
        } else {
            utf16_string(d, o)?
        });
    }
    let mut styles = Vec::with_capacity(style_count);
    for i in 0..style_count {
        let mut o = style_data + u32_at(d, offsets + (count + i) * 4)? as usize;
        let mut spans = Vec::new();
        loop {
            let name = u32_at(d, o)?;
            if name == u32::MAX {
                break;
            }
            spans.push(Span {
                name,
                first: u32_at(d, o + 4)?,
                last: u32_at(d, o + 8)?,
            });
            o += 12;
        }
        styles.push(spans);
    }
    Ok(StringPool {
        utf8,
        sorted: flags & 0x1 != 0,
        strings,
        styles,
    })
}

/// A length of one or two units, the first with the high bit set when there are two.
fn length(d: &[u8], o: &mut usize, wide: bool) -> Result<usize> {
    if wide {
        let a = u16_at(d, *o)? as usize;
        *o += 2;
        if a & 0x8000 == 0 {
            return Ok(a);
        }
        let b = u16_at(d, *o)? as usize;
        *o += 2;
        Ok(((a & 0x7fff) << 16) | b)
    } else {
        let a = u8_at(d, *o)? as usize;
        *o += 1;
        if a & 0x80 == 0 {
            return Ok(a);
        }
        let b = u8_at(d, *o)? as usize;
        *o += 1;
        Ok(((a & 0x7f) << 8) | b)
    }
}

fn utf8_string(d: &[u8], mut o: usize) -> Result<String> {
    length(d, &mut o, false)?; // in UTF-16 units
    let len = length(d, &mut o, false)?;
    let bytes = d.get(o..o + len).ok_or_else(|| past_end(o))?;
    cesu8(bytes).ok_or_else(|| Error::resources(format!("string at {o:#x} is not UTF-8")))
}

/// UTF-8 as Android writes it: a character outside the BMP is a surrogate pair, each half
/// encoded on its own in three bytes.
fn cesu8(b: &[u8]) -> Option<String> {
    if !b.contains(&0xed) {
        return String::from_utf8(b.to_vec()).ok();
    }
    let mut units = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (c, n) = match b[i] {
            x @ 0..0x80 => (x as u32, 1),
            x @ 0xc0..0xe0 => ((x as u32 & 0x1f) << 6 | (*b.get(i + 1)? as u32 & 0x3f), 2),
            x @ 0xe0..0xf0 => (
                (x as u32 & 0x0f) << 12
                    | (*b.get(i + 1)? as u32 & 0x3f) << 6
                    | (*b.get(i + 2)? as u32 & 0x3f),
                3,
            ),
            _ => return None,
        };
        units.push(c as u16);
        i += n;
    }
    String::from_utf16(&units).ok()
}

fn utf16_string(d: &[u8], mut o: usize) -> Result<String> {
    let len = length(d, &mut o, true)?;
    let units = (0..len)
        .map(|k| u16_at(d, o + k * 2))
        .collect::<Result<Vec<_>>>()?;
    String::from_utf16(&units)
        .map_err(|_| Error::resources(format!("string at {o:#x} is not UTF-16")))
}

fn package(d: &[u8], at: usize) -> Result<Package> {
    let (_, header, size) = chunk(d, at)?;
    let name: Vec<u16> = (0..128)
        .map(|k| u16_at(d, at + 12 + k * 2))
        .collect::<Result<_>>()?;
    let name = String::from_utf16_lossy(&name);
    let types_at = at + u32_at(d, at + 268)? as usize;
    let keys_at = at + u32_at(d, at + 276)? as usize;
    let mut chunks = Vec::new();
    let mut c = at + header;
    while c < at + size {
        let (ty, _, len) = chunk(d, c)?;
        if c != types_at && c != keys_at {
            chunks.push(match ty {
                RES_TABLE_TYPE_SPEC => Chunk::Spec(spec(d, c)?),
                RES_TABLE_TYPE => Chunk::Type(type_chunk(d, c)?),
                _ => Chunk::Other(d[c..c + len].to_vec()),
            });
        }
        c += len;
    }
    Ok(Package {
        id: u32_at(d, at + 8)?,
        name: name.trim_end_matches('\0').to_owned(),
        types: pool(d, types_at)?,
        last_public_type: u32_at(d, at + 272)?,
        keys: pool(d, keys_at)?,
        last_public_key: u32_at(d, at + 280)?,
        type_id_offset: if header >= 288 {
            u32_at(d, at + 284)?
        } else {
            0
        },
        chunks,
    })
}

fn spec(d: &[u8], c: usize) -> Result<Spec> {
    let count = u32_at(d, c + 12)? as usize;
    let header = u16_at(d, c + 2)? as usize;
    Ok(Spec {
        id: u8_at(d, c + 8)?,
        res0: u8_at(d, c + 9)?,
        res1: u16_at(d, c + 10)?,
        flags: (0..count)
            .map(|k| u32_at(d, c + header + k * 4))
            .collect::<Result<_>>()?,
    })
}

fn type_chunk(d: &[u8], c: usize) -> Result<Type> {
    let header = u16_at(d, c + 2)? as usize;
    let flags = u8_at(d, c + 9)?;
    let count = u32_at(d, c + 12)? as usize;
    let data = c + u32_at(d, c + 16)? as usize;
    let config_size = u32_at(d, c + 20)? as usize;
    let config = d
        .get(c + 20..c + 20 + config_size)
        .ok_or_else(|| past_end(c + 20))?
        .to_vec();
    let offsets = c + header;
    let mut entries: Vec<Option<Entry>> = Vec::new();
    for k in 0..count {
        let (index, offset) = if flags & Type::SPARSE != 0 {
            (
                u16_at(d, offsets + k * 4)? as usize,
                Some(u16_at(d, offsets + k * 4 + 2)? as usize * 4),
            )
        } else if flags & Type::OFFSET16 != 0 {
            let o = u16_at(d, offsets + k * 2)?;
            (k, (o != 0xffff).then_some(o as usize * 4))
        } else {
            let o = u32_at(d, offsets + k * 4)?;
            (k, (o != u32::MAX).then_some(o as usize))
        };
        if entries.len() <= index {
            entries.resize(index + 1, None);
        }
        if let Some(offset) = offset {
            entries[index] = Some(entry(d, data + offset)?);
        }
    }
    Ok(Type {
        id: u8_at(d, c + 8)?,
        flags,
        reserved: u16_at(d, c + 10)?,
        config,
        entries,
    })
}

fn entry(d: &[u8], e: usize) -> Result<Entry> {
    let flags = u16_at(d, e + 2)?;
    if flags & Entry::COMPACT != 0 {
        return Ok(Entry {
            flags: flags & 0xff,
            key: u16_at(d, e)? as u32,
            value: EntryValue::Simple(Value {
                ty: (flags >> 8) as u8,
                data: u32_at(d, e + 4)?,
            }),
        });
    }
    let size = u16_at(d, e)? as usize;
    let key = u32_at(d, e + 4)?;
    let value = if flags & Entry::COMPLEX != 0 {
        let count = u32_at(d, e + 12)? as usize;
        let map = (0..count)
            .map(|i| {
                let it = e + size + i * 12;
                Ok((u32_at(d, it)?, value(d, it + 4)?))
            })
            .collect::<Result<_>>()?;
        EntryValue::Complex {
            parent: u32_at(d, e + 8)?,
            map,
        }
    } else {
        EntryValue::Simple(value(d, e + size)?)
    };
    Ok(Entry { flags, key, value })
}

fn value(d: &[u8], v: usize) -> Result<Value> {
    Ok(Value {
        ty: u8_at(d, v + 3)?,
        data: u32_at(d, v + 4)?,
    })
}
