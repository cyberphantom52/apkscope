//! The string resources of a table: listed for the viewer, and changed by
//! `apkscope build --string`.

use crate::table::{self, EntryValue, Value, config};
use crate::{Error, Result};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct StringRes {
    /// `-en-rUS`, empty for the default configuration.
    pub config: String,
    pub name: String,
    /// the resource text
    pub value: String,
}

/// Every simple `string` resource of the app package, in every configuration.
pub fn strings(d: &[u8]) -> Result<Vec<StringRes>> {
    let t = table::read(d)?;
    let Some(p) = t.app() else {
        return Ok(vec![]);
    };
    let Some(ty) = p.type_id("string") else {
        return Ok(vec![]);
    };
    let mut out = Vec::new();
    for c in &p.chunks {
        let table::Chunk::Type(types) = c else {
            continue;
        };
        if types.id != ty {
            continue;
        }
        let config = match config::name(&types.config) {
            q if q.is_empty() => q,
            q => format!("-{q}"),
        };
        for e in types.entries.iter().flatten() {
            if let EntryValue::Simple(Value {
                ty: Value::STRING,
                data,
            }) = e.value
            {
                out.push(StringRes {
                    config: config.clone(),
                    name: p
                        .keys
                        .strings
                        .get(e.key as usize)
                        .cloned()
                        .unwrap_or_default(),
                    value: t
                        .strings
                        .strings
                        .get(data as usize)
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
    }
    Ok(out)
}

/// Change string values. `changes`: (config as [`StringRes::config`] has it, name) -> new text.
pub fn set_strings(d: &[u8], changes: &BTreeMap<(String, String), String>) -> Result<Vec<u8>> {
    let mut t = table::read(d)?;
    for ((qualifiers, name), text) in changes {
        let missing = || {
            Error::resources(format!(
                "no string resource {name} in config \"{qualifiers}\""
            ))
        };
        let at = config::parse(qualifiers.trim_start_matches('-'))?;
        let text = t.strings.intern(text);
        let p = t.app_mut().ok_or_else(missing)?;
        let id = p.find("string", name).ok_or_else(missing)?;
        if p.get(id, &at).is_none() {
            return Err(missing());
        }
        p.set(
            id,
            &at,
            EntryValue::Simple(Value {
                ty: Value::STRING,
                data: text,
            }),
        )?;
    }
    Ok(table::write(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // the whole table still reads
        let t = crate::table::read(&out).unwrap();
        assert!(!t.strings.strings.is_empty() && name.value != "Zestle (apkscope) – ünïcode");
    }
}
