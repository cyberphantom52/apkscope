//! Binary XML (a compiled `AndroidManifest.xml`, layout, drawable, ...) as a model: [`read`]
//! it, change it, and [`write`] it back. Writing what was read gives the same file.
//!
//! Format: `ResXMLTree` in `frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h`.

pub mod read;
pub mod text;
pub mod write;

pub use read::read;
pub use write::write;

use crate::table::{StringPool, Value};

/// No string, in a node's `comment` or an attribute's `raw` value.
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// Every name, namespace and string value the document uses.
    pub strings: StringPool,
    /// The resource ID of each of the first strings, for attribute names: `android:layout_width`
    /// is named by both its string and `0x010100f4`.
    pub ids: Vec<u32>,
    /// The document's nodes, in order.
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    StartNamespace(Namespace),
    EndNamespace(Namespace),
    Start(Start),
    End(End),
    Text(Text),
    /// Kept as read.
    Other(Vec<u8>),
}

/// Where a node was in the source, and the comment before it ([`NONE`] for none).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Source {
    pub line: u32,
    pub comment: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Namespace {
    pub source: Source,
    pub prefix: u32,
    pub uri: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    pub source: Source,
    /// The namespace URI ([`NONE`] for none) and name.
    pub ns: u32,
    pub name: u32,
    /// One more than the index of the `id`, `class` and `style` attributes, 0 if there is none.
    pub id_index: u16,
    pub class_index: u16,
    pub style_index: u16,
    pub attrs: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub ns: u32,
    pub name: u32,
    /// The value as written, for a string ([`NONE`] otherwise).
    pub raw: u32,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct End {
    pub source: Source,
    pub ns: u32,
    pub name: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub source: Source,
    pub data: u32,
    pub value: Value,
    /// The typed value's size field: 8, or 0 where an older tool left the value empty.
    pub value_size: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every binary XML file under a directory, and in every .apk there, writes back to the
    /// same bytes:
    /// APKSCOPE_XML=<dir> cargo test --release xml_round_trip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn xml_round_trip() {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
            use std::io::Read;
            for e in std::fs::read_dir(dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|x| x == "apk") {
                    let mut z = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
                    for i in 0..z.len() {
                        let mut e = z.by_index(i).unwrap();
                        let mut b = vec![];
                        if e.read_to_end(&mut b).is_ok() && b.starts_with(&[3, 0, 8, 0]) {
                            out.push((
                                format!("{}!{}", path.display(), e.name().unwrap_or_default()),
                                b,
                            ));
                        }
                    }
                } else {
                    let b = std::fs::read(&path).unwrap();
                    if b.starts_with(&[3, 0, 8, 0]) {
                        out.push((path.display().to_string(), b));
                    }
                }
            }
        }
        let mut files = vec![];
        walk(
            std::env::var("APKSCOPE_XML")
                .expect("set APKSCOPE_XML")
                .as_ref(),
            &mut files,
        );
        let mut differ = 0;
        for (name, bytes) in &files {
            let doc = read(bytes).unwrap_or_else(|e| panic!("{name}: {}", e.report()));
            if write(&doc) != *bytes {
                differ += 1;
                if differ <= 5 {
                    let at = write(&doc).iter().zip(bytes).position(|(a, b)| a != b);
                    println!(
                        "{name}: differs at {at:?} ({} vs {} bytes)",
                        write(&doc).len(),
                        bytes.len()
                    );
                }
            }
        }
        println!("{} files, {differ} write back differently", files.len());
        assert_eq!(differ, 0);
    }
}
