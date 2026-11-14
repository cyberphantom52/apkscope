//! Text XML -> Android binary XML (the format of a compiled AndroidManifest.xml), so an edited
//! manifest can go back into an APK. With the original file, whatever the text leaves as it
//! was stays exactly as it is there, so `versionName="1"` stays a string.

use crate::Result;
use crate::res::Names;
use crate::table::StringPool;
use crate::xml::{self, Document};

pub fn encode(text: &str, original: Option<&[u8]>, names: &Names) -> Result<Vec<u8>> {
    let original = match original {
        Some(bytes) => xml::read(bytes)?,
        None => Document {
            strings: StringPool {
                utf8: true,
                sorted: false,
                strings: vec![],
                styles: vec![],
            },
            ids: vec![],
            nodes: vec![],
        },
    };
    Ok(xml::write(&xml::text::apply(&original, text, names)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::res::decode_xml;
    use crate::table::Table;

    fn names() -> Names {
        Names::new(&Table {
            strings: StringPool {
                utf8: true,
                sorted: false,
                strings: vec![],
                styles: vec![],
            },
            packages: vec![],
        })
    }

    const MANIFEST: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.example" android:versionCode="3" android:versionName="1.0">
    <application android:label="@android:string/ok" android:debuggable="true" />
</manifest>"#;

    #[test]
    fn encodes_text_alone() {
        let names = names();
        let bin = encode(MANIFEST, None, &names).unwrap();
        let text = decode_xml(&bin, Some(&names)).unwrap();
        for want in [
            r#"package="com.example""#,
            r#"android:versionCode="3""#,
            r#"android:label="@android:string/ok""#,
            r#"android:debuggable="true""#,
        ] {
            assert!(text.contains(want), "{want} in\n{text}");
        }
        // framework attributes are named by ID too, in the pool's first strings
        let doc = xml::read(&bin).unwrap();
        assert!(doc.ids.contains(&0x0101_021b)); // versionCode
        assert!(encode("<a><b></a>", None, &names).is_err());
    }

    #[test]
    fn edits_keep_the_rest() {
        let names = names();
        let bin = encode(MANIFEST, None, &names).unwrap();
        let text = decode_xml(&bin, Some(&names)).unwrap();
        // the same text gives the same bytes
        assert_eq!(encode(&text, Some(&bin), &names).unwrap(), bin);
        let edited = text
            .replace(r#"android:versionCode="3""#, r#"android:versionCode="4""#)
            .replace(
                r#"android:debuggable="true""#,
                r#"android:debuggable="true"
        android:allowBackup="false""#,
            );
        let after =
            decode_xml(&encode(&edited, Some(&bin), &names).unwrap(), Some(&names)).unwrap();
        assert!(after.contains(r#"android:versionCode="4""#), "{after}");
        assert!(after.contains(r#"android:allowBackup="false""#), "{after}");
        assert!(after.contains(r#"android:versionName="1.0""#), "{after}");
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
