//! An APK's resources as files, laid out as apktool lays them out: `AndroidManifest.xml` and
//! `res/`, values as values XML, binary XML as text, other files as they are; and files like
//! those, edited, back into the APK.

use crate::apk::Apk;
use crate::res::{self, Names};
use crate::{Error, Result, rebuild, table, values, xml};
use std::collections::BTreeMap;
use std::io::{Read, Seek};

const MANIFEST: &str = "AndroidManifest.xml";

/// What [`decode`] makes of an APK, and where each file came from.
struct Decoded {
    /// By path: `AndroidManifest.xml`, `res/values/strings.xml`, `res/drawable-hdpi/icon.png`.
    files: BTreeMap<String, Vec<u8>>,
    /// A file resource's path in the APK, by its path here: `res/layout/main.xml` may be
    /// `res/Wb5.xml` in an APK whose resource files were renamed.
    sources: BTreeMap<String, String>,
    table: Option<table::Table>,
    names: Names,
}

fn decoded<R: Read + Seek>(apk: &mut Apk<R>) -> Result<Decoded> {
    let table = apk
        .read("resources.arsc")
        .map(|d| table::read(&d))
        .transpose()?;
    let names = Names::new(table.as_ref().unwrap_or(&table::Table {
        strings: table::StringPool {
            utf8: true,
            sorted: false,
            strings: vec![],
            styles: vec![],
        },
        packages: vec![],
    }));
    let mut files = BTreeMap::new();
    let mut sources = BTreeMap::new();
    if let Some(m) = apk.read(MANIFEST) {
        files.insert(
            MANIFEST.to_owned(),
            res::decode_xml(&m, Some(&names))?.into_bytes(),
        );
    }
    if let Some(t) = &table {
        let d = res::decode_table(t, &names);
        for (path, text) in d.values {
            files.insert(format!("res/{path}"), text.into_bytes());
        }
        for (path, src) in d.files {
            let Some(data) = apk.read(&src) else { continue };
            let data = match xml::read(&data) {
                Ok(doc) => xml::text::decode(&doc, &names).into_bytes(),
                Err(_) => data,
            };
            files.insert(format!("res/{path}"), data);
            sources.insert(format!("res/{path}"), src);
        }
    }
    Ok(Decoded {
        files,
        sources,
        table,
        names,
    })
}

/// The APK's resources as files by path: `AndroidManifest.xml`, `res/values/strings.xml`,
/// `res/layout/main.xml` as text, `res/drawable-hdpi/icon.png` as it is.
pub fn decode<R: Read + Seek>(apk: &mut Apk<R>) -> Result<BTreeMap<String, Vec<u8>>> {
    Ok(decoded(apk)?.files)
}

/// How the APK's entries change for `files`, laid out as [`decode`] lays them out, to be its
/// resources. A file that is as [`decode`] gives it changes nothing. With `complete`, `files`
/// are all of them: a values file it doesn't have has no values.
pub fn edits<R: Read + Seek>(
    apk: &mut Apk<R>,
    files: &BTreeMap<String, Vec<u8>>,
    complete: bool,
) -> Result<rebuild::Edits> {
    let d = decoded(apk)?;
    let mut edits = rebuild::Edits::new();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let text = |path: &str, bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::xml(format!("{path} is not UTF-8")))
    };
    for (path, bytes) in files {
        if d.files.get(path) == Some(bytes) && path != "res/values/public.xml" {
            continue;
        }
        if path == MANIFEST {
            let original = apk
                .read(MANIFEST)
                .ok_or_else(|| Error::EntryNotFound(MANIFEST.into()))?;
            let doc = xml::text::apply(&xml::read(&original)?, &text(path, bytes)?, &d.names)
                .map_err(|e| e.at(path))?;
            edits.insert(MANIFEST.into(), Some(xml::write(&doc)));
        } else if let Some(v) = path
            .strip_prefix("res/")
            .filter(|p| p.starts_with("values"))
        {
            values.insert(v.to_owned(), text(path, bytes)?);
        } else if let Some(src) = d.sources.get(path) {
            let original = apk
                .read(src)
                .ok_or_else(|| Error::EntryNotFound(src.clone()))?;
            let new = match xml::read(&original) {
                Ok(doc) => xml::write(
                    &xml::text::apply(&doc, &text(path, bytes)?, &d.names)
                        .map_err(|e| e.at(path))?,
                ),
                Err(_) => bytes.clone(),
            };
            edits.insert(src.clone(), Some(new));
        } else {
            return Err(Error::EntryNotFound(format!(
                "{path}: no resource of the APK is there; adding resource files is not supported yet"
            )));
        }
    }
    if complete {
        // a values file that is gone has no values
        for path in d.files.keys() {
            if let Some(v) = path
                .strip_prefix("res/")
                .filter(|p| p.starts_with("values"))
                && !files.contains_key(path)
                && v != "values/public.xml"
            {
                values.insert(v.to_owned(), "<resources />".into());
            }
        }
    }
    if values.keys().any(|p| p != "values/public.xml") {
        let mut table = d
            .table
            .ok_or_else(|| Error::EntryNotFound("resources.arsc".into()))?;
        values::apply(&mut table, &values)?;
        edits.insert("resources.arsc".into(), Some(table::write(&table)));
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{
        Chunk, Entry, EntryValue, Package, Spec, StringPool, Table, Type, Value, config,
    };
    use std::io::{Cursor, Write};

    fn pool(strings: &[&str]) -> StringPool {
        StringPool {
            utf8: true,
            sorted: false,
            strings: strings.iter().map(|s| s.to_string()).collect(),
            styles: vec![],
        }
    }

    /// An APK with a manifest and one string, `hello`, in the default and night configurations.
    fn apk() -> Vec<u8> {
        let string = |i| {
            Some(Entry {
                flags: 0,
                key: 0,
                value: EntryValue::Simple(Value {
                    ty: Value::STRING,
                    data: i,
                }),
            })
        };
        let ty = |config, i| {
            Chunk::Type(Type {
                id: 1,
                flags: 0,
                reserved: 0,
                config,
                entries: vec![string(i)],
            })
        };
        let table = Table {
            strings: pool(&["Hello", "Good night"]),
            packages: vec![Package {
                id: 0x7f,
                name: "com.example".into(),
                last_public_type: 0,
                last_public_key: 0,
                type_id_offset: 0,
                types: pool(&["string"]),
                keys: pool(&["hello"]),
                chunks: vec![
                    Chunk::Spec(Spec {
                        id: 1,
                        res0: 0,
                        res1: 2,
                        flags: vec![config::bits::UI_MODE],
                    }),
                    ty(config::default(), 0),
                    ty(config::parse("night").unwrap(), 1),
                ],
            }],
        };
        let names = Names::new(&table);
        let manifest = crate::axml::encode(
            r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.example" android:label="@string/hello" />"#,
            None,
            &names,
        )
        .unwrap();
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        z.start_file("AndroidManifest.xml", stored).unwrap();
        z.write_all(&manifest).unwrap();
        z.start_file("resources.arsc", stored).unwrap();
        z.write_all(&table::write(&table)).unwrap();
        z.finish().unwrap().into_inner()
    }

    #[test]
    fn decodes_and_edits_a_tree() {
        let bytes = apk();
        let mut a = Apk::new(Cursor::new(&bytes)).unwrap();
        let files = decode(&mut a).unwrap();
        let text = |p: &str| String::from_utf8(files[p].clone()).unwrap();
        assert!(text("AndroidManifest.xml").contains(r#"android:label="@string/hello""#));
        assert!(text("res/values/strings.xml").contains(r#"<string name="hello">Hello</string>"#));
        assert!(text("res/values-night/strings.xml").contains("Good night"));
        assert!(text("res/values/public.xml").contains(r#"id="0x7f010000""#));

        // what decode gives changes nothing
        assert!(edits(&mut a, &files, true).unwrap().is_empty());

        // an edited string, and a configuration that is gone
        let mut edited = files.clone();
        edited.insert(
            "res/values/strings.xml".into(),
            text("res/values/strings.xml")
                .replace(">Hello<", ">Hi<")
                .into_bytes(),
        );
        edited.remove("res/values-night/strings.xml");
        let e = edits(&mut a, &edited, true).unwrap();
        let t = table::read(e["resources.arsc"].as_ref().unwrap()).unwrap();
        let p = t.app().unwrap();
        let Some(EntryValue::Simple(v)) = p.get(0x7f01_0000, &config::default()).map(|e| &e.value)
        else {
            panic!("a string")
        };
        assert_eq!(t.strings.strings[v.data as usize], "Hi");
        // the night configuration has no values left, and so no chunk
        assert_eq!(p.values(0x7f01_0000).count(), 1);
        assert_eq!(
            p.chunks
                .iter()
                .filter(|c| matches!(c, Chunk::Type(_)))
                .count(),
            1
        );
        // without `complete`, a file that isn't given is left as it is
        let e = edits(&mut a, &edited, false).unwrap();
        let t = table::read(e["resources.arsc"].as_ref().unwrap()).unwrap();
        assert_eq!(t.app().unwrap().values(0x7f01_0000).count(), 2);

        edited.insert("res/drawable/new.png".into(), vec![0]);
        assert!(matches!(
            edits(&mut a, &edited, true),
            Err(Error::EntryNotFound(_))
        ));
    }
}
