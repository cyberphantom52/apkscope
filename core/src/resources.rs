//! An APK's resources as files, laid out as apktool lays them out: `AndroidManifest.xml` and
//! `res/`, values as values XML, binary XML as text, other files as they are; and files like
//! those, edited, back into the APK.

use crate::apk::Apk;
use crate::res::{self, Names};
use crate::table::config;
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

/// The manifest's `minSdkVersion`: 0 if it names none, and for a preview's codename the
/// version aapt2 gives that, 10000.
fn min_sdk<R: Read + Seek>(apk: &mut Apk<R>) -> u16 {
    const MIN_SDK_VERSION: u32 = 0x0101_020c;
    let Some(doc) = apk.read(MANIFEST).and_then(|m| xml::read(&m).ok()) else {
        return 0;
    };
    doc.nodes
        .iter()
        .filter_map(|n| match n {
            xml::Node::Start(s) => Some(s),
            _ => None,
        })
        .filter(|s| {
            doc.strings
                .strings
                .get(s.name as usize)
                .is_some_and(|n| n == "uses-sdk")
        })
        .flat_map(|s| &s.attrs)
        .find(|a| doc.ids.get(a.name as usize) == Some(&MIN_SDK_VERSION))
        .map_or(0, |a| match a.value.ty {
            table::Value::INT_DEC | table::Value::INT_HEX => a.value.data as u16,
            _ => 10000,
        })
}

/// Where a file of a decoded tree, `res/<type>[-<qualifiers>]/<name>.<ext>`, puts its
/// resource: its type, its name and the configuration its directory names in `p`.
fn file_resource(
    p: &table::Package,
    path: &str,
    min_sdk: u16,
) -> Result<(String, String, Vec<u8>)> {
    let bad = || Error::resources(format!("{path}: not res/<type>/<file>"));
    let (dir, file) = path
        .strip_prefix("res/")
        .and_then(|p| p.split_once('/'))
        .filter(|(_, file)| !file.contains('/'))
        .ok_or_else(bad)?;
    let (ty, qualifiers) = dir.split_once('-').unwrap_or((dir, ""));
    let name = file
        .split('.')
        .next()
        .filter(|n| !n.is_empty())
        .ok_or_else(bad)?;
    let config = p.config(qualifiers, min_sdk).map_err(|e| e.at(path))?;
    Ok((ty.to_owned(), name.to_owned(), config))
}

/// How the APK's entries change for `files`, laid out as [`decode`] lays them out, to be its
/// resources. A file that is as [`decode`] gives it changes nothing; a new file under `res/`
/// adds its resource, binary XML compiled from its text where aapt2 would compile it. With
/// `complete`, `files` are all of them: a values file it doesn't have has no values, and a
/// resource file it doesn't have is removed.
pub fn edits<R: Read + Seek>(
    apk: &mut Apk<R>,
    files: &BTreeMap<String, Vec<u8>>,
    complete: bool,
) -> Result<rebuild::Edits> {
    let d = decoded(apk)?;
    let min_sdk = min_sdk(apk);
    let mut table = d.table;
    let mut changed = false;
    let mut edits = rebuild::Edits::new();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    // XML to compile once the table has every new resource: entry, path, text, the original
    let mut xml: Vec<(String, &str, String, Option<Vec<u8>>)> = Vec::new();
    let text = |path: &str, bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::xml(format!("{path} is not UTF-8")))
    };
    let no_table = || Error::EntryNotFound("resources.arsc".into());
    for (path, bytes) in files {
        if d.files.get(path) == Some(bytes) && path != "res/values/public.xml" {
            continue;
        }
        if path == MANIFEST {
            let original = apk
                .read(MANIFEST)
                .ok_or_else(|| Error::EntryNotFound(MANIFEST.into()))?;
            xml.push((MANIFEST.into(), path, text(path, bytes)?, Some(original)));
        } else if let Some(v) = path
            .strip_prefix("res/")
            .filter(|p| p.starts_with("values"))
        {
            values.insert(v.to_owned(), text(path, bytes)?);
        } else if let Some(src) = d.sources.get(path) {
            let original = apk
                .read(src)
                .ok_or_else(|| Error::EntryNotFound(src.clone()))?;
            match xml::read(&original) {
                Ok(_) => xml.push((src.clone(), path, text(path, bytes)?, Some(original))),
                Err(_) => {
                    edits.insert(src.clone(), Some(bytes.clone()));
                }
            }
        } else if path.starts_with("res/") {
            let t = table.as_mut().ok_or_else(no_table)?;
            let p = t.app().ok_or_else(no_table)?;
            let (ty, name, config) = file_resource(p, path, min_sdk)?;
            // the entry is named as aapt2 names it, by the configuration it is in
            let file = path.rsplit('/').next().expect("a file name");
            let entry = match config::name(&config).as_str() {
                "" => format!("res/{ty}/{file}"),
                q => format!("res/{ty}-{q}/{file}"),
            };
            let value = table::EntryValue::Simple(table::Value {
                ty: table::Value::STRING,
                data: t.strings.intern(&entry),
            });
            let p = t.app_mut().expect("the app package");
            match p.find(&ty, &name) {
                Some(id) => p.set(id, &config, value)?,
                None => {
                    p.add(&ty, &name, &config, value)?;
                }
            }
            changed = true;
            // aapt2 compiles XML everywhere but in raw/
            if file.ends_with(".xml") && ty != "raw" {
                xml.push((entry, path, text(path, bytes)?, None));
            } else {
                edits.insert(entry, Some(bytes.clone()));
            }
        } else {
            return Err(Error::EntryNotFound(format!(
                "{path}: not a resource; resources are AndroidManifest.xml and res/"
            )));
        }
    }
    if complete {
        for path in d.files.keys().filter(|p| !files.contains_key(*p)) {
            if let Some(v) = path
                .strip_prefix("res/")
                .filter(|p| p.starts_with("values"))
            {
                // a values file that is gone has no values
                if v != "values/public.xml" {
                    values.insert(v.to_owned(), "<resources />".into());
                }
            } else if let Some(src) = d.sources.get(path) {
                // a resource file that is gone has no resource there
                let t = table.as_mut().ok_or_else(no_table)?;
                let p = t.app().ok_or_else(no_table)?;
                let (ty, name, config) = file_resource(p, path, min_sdk)?;
                let at = t.strings.index(src);
                let p = t.app_mut().expect("the app package");
                // unless a new file took its place
                if let Some(id) = p.find(&ty, &name)
                    && let Some(e) = p.get(id, &config)
                    && matches!(&e.value, table::EntryValue::Simple(v)
                        if v.ty == table::Value::STRING && Some(v.data) == at)
                {
                    p.remove(id, &config);
                    changed = true;
                }
                edits.insert(src.clone(), None);
            }
        }
    }
    if values.keys().any(|p| p != "values/public.xml") {
        values::apply(table.as_mut().ok_or_else(no_table)?, &values, min_sdk)?;
        changed = true;
    }
    // names for the XML: the table's as edited, so it can name what was just added
    let names = match (&table, changed) {
        (Some(t), true) => Names::new(t),
        _ => d.names,
    };
    for (entry, path, text, original) in xml {
        let original = match original {
            Some(bytes) => xml::read(&bytes)?,
            None => xml::Document {
                strings: table::StringPool {
                    utf8: true,
                    sorted: false,
                    strings: vec![],
                    styles: vec![],
                },
                ids: vec![],
                nodes: vec![],
            },
        };
        let doc = xml::text::apply(&original, &text, &names).map_err(|e| e.at(path))?;
        edits.insert(entry, Some(xml::write(&doc)));
    }
    if changed && let Some(t) = &table {
        edits.insert("resources.arsc".into(), Some(table::write(t)));
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

        // new files: an image, and XML that names a string added with it
        let mut added = files.clone();
        added.insert("res/drawable-xxhdpi/icon.png".into(), b"png".to_vec());
        added.insert(
            "res/xml/prefs.xml".into(),
            br#"<PreferenceScreen xmlns:android="http://schemas.android.com/apk/res/android" android:title="@string/added" />"#.to_vec(),
        );
        added.insert(
            "res/values/strings.xml".into(),
            text("res/values/strings.xml")
                .replace(
                    "</resources>",
                    r#"<string name="added">Added</string></resources>"#,
                )
                .into_bytes(),
        );
        let e = edits(&mut a, &added, true).unwrap();
        // the image's directory is named as aapt2 compiles it, with the version density needs
        assert_eq!(
            e["res/drawable-xxhdpi-v4/icon.png"].as_deref(),
            Some(&b"png"[..])
        );
        let t = table::read(e["resources.arsc"].as_ref().unwrap()).unwrap();
        let names = Names::new(&t);
        let p = t.app().unwrap();
        let icon = p.find("drawable", "icon").unwrap();
        let xxhdpi = config::parse("xxhdpi-v4").unwrap();
        let Some(EntryValue::Simple(v)) = p.get(icon, &xxhdpi).map(|e| &e.value) else {
            panic!("a file")
        };
        assert_eq!(
            t.strings.strings[v.data as usize],
            "res/drawable-xxhdpi-v4/icon.png"
        );
        let prefs =
            res::decode_xml(e["res/xml/prefs.xml"].as_ref().unwrap(), Some(&names)).unwrap();
        assert!(
            prefs.contains(r#"android:title="@string/added""#),
            "{prefs}"
        );

        // a decoded tree of that APK, with the image gone, removes it
        let rebuilt = rebuild::rebuild_from(Cursor::new(&bytes), &e).unwrap().0;
        let mut b = Apk::new(Cursor::new(&rebuilt)).unwrap();
        let mut tree = decode(&mut b).unwrap();
        assert!(tree.contains_key("res/drawable-xxhdpi-v4/icon.png"));
        tree.remove("res/drawable-xxhdpi-v4/icon.png");
        let e = edits(&mut b, &tree, true).unwrap();
        assert_eq!(e["res/drawable-xxhdpi-v4/icon.png"], None);
        let t = table::read(e["resources.arsc"].as_ref().unwrap()).unwrap();
        let p = t.app().unwrap();
        // its only value, and with it the type's only configuration, is gone
        assert!(
            p.find("drawable", "icon")
                .is_none_or(|id| p.get(id, &xxhdpi).is_none())
        );

        // anything else is not a resource
        edited.insert("assets/x".into(), vec![0]);
        assert!(matches!(
            edits(&mut a, &edited, true),
            Err(Error::EntryNotFound(_))
        ));
    }
}
