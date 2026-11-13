//! Edit string resources in resources.arsc: each changed value is appended to the global string
//! pool and only that entry is pointed at it, so other resources sharing the old text keep it.

use std::collections::BTreeMap;

use crate::res::{qualifier, string_pool};
use crate::{Error, Result};

fn u16_at(d: &[u8], o: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        d.get(o..o + 2)
            .ok_or_else(|| Error::resources("truncated resources.arsc"))?
            .try_into()
            .unwrap(),
    ))
}

fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        d.get(o..o + 4)
            .ok_or_else(|| Error::resources("truncated resources.arsc"))?
            .try_into()
            .unwrap(),
    ))
}

/// One string resource: config qualifier ("" = default), name, value, and where its
/// Res_value sits in the file (offset of the type byte).
#[derive(Clone, Debug)]
pub struct StringRes {
    pub config: String,
    pub name: String,
    /// the resource text
    pub value: String,
    at: usize,
}

/// Every simple `string` resource of the first package, in every config.
pub fn strings(d: &[u8]) -> Result<Vec<StringRes>> {
    if u16_at(d, 0)? != 0x0002 {
        return Err(Error::resources("not a resource table"));
    }
    let mut values = Vec::new();
    let mut out = Vec::new();
    let mut at = u16_at(d, 2)? as usize;
    while at + 8 <= d.len() {
        let (ty, size) = (u16_at(d, at)?, u32_at(d, at + 4)? as usize);
        if size < 8 {
            break;
        }
        match ty {
            0x0001 => values = string_pool(d, at)?,
            0x0200 => {
                let types = string_pool(d, at + u32_at(d, at + 268)? as usize)?;
                let keys = string_pool(d, at + u32_at(d, at + 276)? as usize)?;
                let end = at + size;
                let mut c = at + u16_at(d, at + 2)? as usize;
                while c + 8 <= end.min(d.len()) {
                    let (cty, csize) = (u16_at(d, c)?, u32_at(d, c + 4)? as usize);
                    if csize < 8 {
                        break;
                    }
                    if cty == 0x0201
                        && types.get(d[c + 8] as usize - 1).map(String::as_str) == Some("string")
                    {
                        type_strings(d, c, &keys, &values, &mut out)?;
                    }
                    c += csize;
                }
                // shortcut: only the first package, like the decoder
                break;
            }
            _ => {}
        }
        at += size;
    }
    Ok(out)
}

fn type_strings(
    d: &[u8],
    c: usize,
    keys: &[String],
    values: &[String],
    out: &mut Vec<StringRes>,
) -> Result<()> {
    let flags = d[c + 9];
    let count = u32_at(d, c + 12)? as usize;
    let entries = c + u32_at(d, c + 16)? as usize;
    let config = qualifier(d, c + 20)?;
    let offsets = c + u16_at(d, c + 2)? as usize;
    let mut list = Vec::new();
    for k in 0..count {
        let o = if flags & 0x01 != 0 {
            Some(u16_at(d, offsets + k * 4 + 2)? as usize * 4)
        } else if flags & 0x02 != 0 {
            Some(u16_at(d, offsets + k * 2)?)
                .filter(|&o| o != 0xffff)
                .map(|o| o as usize * 4)
        } else {
            Some(u32_at(d, offsets + k * 4)?)
                .filter(|&o| o != u32::MAX)
                .map(|o| o as usize)
        };
        list.extend(o);
    }
    for o in list {
        let e = entries + o;
        let eflags = u16_at(d, e + 2)?;
        // compact entries keep the type in the flags and can't be repointed in place
        if eflags & 0x09 != 0 {
            continue;
        }
        let key = u32_at(d, e + 4)? as usize;
        let v = e + u16_at(d, e)? as usize;
        if d.get(v + 3) == Some(&0x03) {
            let idx = u32_at(d, v + 4)? as usize;
            out.push(StringRes {
                config: config.clone(),
                name: keys.get(key).cloned().unwrap_or_default(),
                value: values.get(idx).cloned().unwrap_or_default(),
                at: v + 3,
            });
        }
    }
    Ok(())
}

/// Encoded string for a pool: UTF-8 (two lengths) or UTF-16 (one length, NUL-terminated).
fn encode(s: &str, utf8: bool) -> Vec<u8> {
    let mut o = Vec::new();
    let units: Vec<u16> = s.encode_utf16().collect();
    if utf8 {
        let len8 = |o: &mut Vec<u8>, n: usize| {
            if n > 0x7f {
                o.push(0x80 | (n >> 8) as u8);
            }
            o.push((n & 0xff) as u8);
        };
        len8(&mut o, units.len());
        len8(&mut o, s.len());
        o.extend_from_slice(s.as_bytes());
        o.push(0);
    } else {
        if units.len() > 0x7fff {
            o.extend_from_slice(&((0x8000 | (units.len() >> 16)) as u16).to_le_bytes());
        }
        o.extend_from_slice(&(units.len() as u16).to_le_bytes());
        units
            .iter()
            .for_each(|u| o.extend_from_slice(&u.to_le_bytes()));
        o.extend_from_slice(&[0, 0]);
    }
    o
}

/// Change string values. `changes`: (config, name) -> new text.
pub fn set_strings(d: &[u8], changes: &BTreeMap<(String, String), String>) -> Result<Vec<u8>> {
    let all = strings(d)?;
    let mut targets = Vec::new();
    for ((config, name), text) in changes {
        let r = all
            .iter()
            .find(|r| &r.config == config && &r.name == name)
            .ok_or_else(|| {
                Error::resources(format!("no string resource {name} in config \"{config}\""))
            })?;
        targets.push((r.at, text.clone()));
    }
    // the global string pool is the table's first chunk
    let pool = u16_at(d, 2)? as usize;
    if u16_at(d, pool)? != 0x0001 {
        return Err(Error::resources(
            "resources.arsc has no global string pool first",
        ));
    }
    let (psize, count, styles, flags) = (
        u32_at(d, pool + 4)? as usize,
        u32_at(d, pool + 8)? as usize,
        u32_at(d, pool + 12)? as usize,
        u32_at(d, pool + 16)?,
    );
    let (sstart, ystart) = (
        u32_at(d, pool + 20)? as usize,
        u32_at(d, pool + 24)? as usize,
    );
    let hsize = u16_at(d, pool + 2)? as usize;
    let utf8 = flags & 0x100 != 0;
    let str_offsets: Vec<u32> = (0..count)
        .map(|k| u32_at(d, pool + hsize + k * 4))
        .collect::<Result<_, _>>()?;
    let style_offsets: Vec<u32> = (0..styles)
        .map(|k| u32_at(d, pool + hsize + count * 4 + k * 4))
        .collect::<Result<_, _>>()?;
    let str_end = if ystart != 0 { ystart } else { psize };
    let mut sdata = d[pool + sstart..pool + str_end].to_vec();
    let ydata = if ystart != 0 {
        d[pool + ystart..pool + psize].to_vec()
    } else {
        Vec::new()
    };
    let mut new_offsets = str_offsets.clone();
    for (_, text) in &targets {
        new_offsets.push(sdata.len() as u32);
        sdata.extend(encode(text, utf8));
    }
    while !sdata.len().is_multiple_of(4) {
        sdata.push(0);
    }
    let n = new_offsets.len();
    let new_sstart = hsize + 4 * n + 4 * styles;
    let new_ystart = if ystart != 0 {
        new_sstart + sdata.len()
    } else {
        0
    };
    let mut chunk = d[pool..pool + hsize].to_vec();
    let new_size = new_sstart + sdata.len() + ydata.len();
    chunk[4..8].copy_from_slice(&(new_size as u32).to_le_bytes());
    chunk[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    chunk[20..24].copy_from_slice(&(new_sstart as u32).to_le_bytes());
    chunk[24..28].copy_from_slice(&(new_ystart as u32).to_le_bytes());
    new_offsets
        .iter()
        .for_each(|o| chunk.extend_from_slice(&o.to_le_bytes()));
    style_offsets
        .iter()
        .for_each(|o| chunk.extend_from_slice(&o.to_le_bytes()));
    chunk.extend(sdata);
    chunk.extend(ydata);
    let delta = chunk.len() as isize - psize as isize;
    let mut out = Vec::with_capacity(d.len() + chunk.len());
    out.extend_from_slice(&d[..pool]);
    out.extend(chunk);
    out.extend_from_slice(&d[pool + psize..]);
    let total = out.len() as u32;
    out[4..8].copy_from_slice(&total.to_le_bytes());
    // point each changed entry at its new string (everything after the pool moved by delta)
    for (k, (at, _)) in targets.iter().enumerate() {
        let p = (*at as isize + delta) as usize;
        out[p] = 0x03;
        out[p + 1..p + 5].copy_from_slice(&((count + k) as u32).to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_encodings() {
        assert_eq!(encode("hi", true), vec![2, 2, b'h', b'i', 0]);
        assert_eq!(encode("hi", false), vec![2, 0, b'h', 0, b'i', 0, 0, 0]);
        // long UTF-8 strings use two-byte lengths
        assert_eq!(
            &encode(&"x".repeat(200), true)[..4],
            &[0x80, 200, 0x80, 200]
        );
    }

    /// change app_name in a real table, then read it back with the regular decoder
    #[test]
    fn real_table_edit() {
        let apk = "D:/APKS/Zestle_v0.6.8-prealpha.apk";
        if !std::path::Path::new(apk).exists() {
            return;
        }
        let d = crate::apk::open(apk)
            .unwrap()
            .read("resources.arsc")
            .unwrap();
        let all = strings(&d).unwrap();
        let name = all
            .iter()
            .find(|r| r.config.is_empty() && r.name == "app_name")
            .expect("app_name")
            .clone();
        let other = all
            .iter()
            .find(|r| r.config.is_empty() && r.name != "app_name")
            .unwrap()
            .clone();
        let mut ch = BTreeMap::new();
        ch.insert(
            (String::new(), "app_name".to_string()),
            "Zestle (apkscope) – ünïcode".to_string(),
        );
        let out = set_strings(&d, &ch).unwrap();
        let after = strings(&out).unwrap();
        assert_eq!(
            after
                .iter()
                .find(|r| r.config.is_empty() && r.name == "app_name")
                .unwrap()
                .value,
            "Zestle (apkscope) – ünïcode"
        );
        // everything else unchanged
        assert_eq!(
            after
                .iter()
                .find(|r| r.config == other.config && r.name == other.name)
                .unwrap()
                .value,
            other.value
        );
        assert_eq!(after.len(), all.len());
        // the full decoder still reads the table
        let t = crate::res::parse_table(&out).unwrap();
        assert!(!t.values.is_empty() && name.value != "Zestle (apkscope) – ünïcode");
    }
}
