//! `resources.arsc` as a model: [`read`] it, change it, and [`write`] it back. Writing what was
//! read gives the same file, so an edit changes only what it touches.
//!
//! Format: `frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h` in AOSP.

pub mod read;
pub mod write;

pub use read::read;
pub use write::write;

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// The global pool: string values, and the names of style spans.
    pub strings: StringPool,
    pub packages: Vec<Package>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StringPool {
    pub utf8: bool,
    pub sorted: bool,
    pub strings: Vec<String>,
    /// Style spans of the first strings, in order: one list per styled string.
    pub styles: Vec<Vec<Span>>,
}

/// Markup over a string's characters `first..=last` (UTF-16 units), named by a string in the
/// same pool: `b`, `i`, `font;size=12`, ...
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub name: u32,
    pub first: u32,
    pub last: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    /// `0x7f` for an app, `0x01` for the framework.
    pub id: u32,
    pub name: String,
    pub last_public_type: u32,
    pub last_public_key: u32,
    pub type_id_offset: u32,
    /// Type names: `attr`, `drawable`, `string`, ...; type `n` is `types[n - 1]`.
    pub types: StringPool,
    /// Entry names.
    pub keys: StringPool,
    /// The package's chunks in file order.
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    Spec(Spec),
    Type(Type),
    /// Kept as read: the library, overlayable and staged-alias chunks.
    Other(Vec<u8>),
}

/// Which configurations each entry of a type varies by, and whether it is public.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub id: u8,
    pub res0: u8,
    /// Since Android 13, the number of the type's [`Type`] chunks.
    pub res1: u16,
    pub flags: Vec<u32>,
}

/// A type's entries for one configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    pub id: u8,
    /// [`Type::SPARSE`], [`Type::OFFSET16`]: how the entries are indexed.
    pub flags: u8,
    pub reserved: u16,
    /// `ResTable_config`, as read.
    pub config: Vec<u8>,
    /// By entry index; a sparse type has only the entries that are there.
    pub entries: Vec<Option<Entry>>,
}

impl Type {
    pub const SPARSE: u8 = 0x01;
    pub const OFFSET16: u8 = 0x02;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// [`Entry::COMPLEX`], [`Entry::PUBLIC`], [`Entry::WEAK`], [`Entry::COMPACT`].
    pub flags: u16,
    /// Index into the package's [`Package::keys`].
    pub key: u32,
    pub value: EntryValue,
}

impl Entry {
    pub const COMPLEX: u16 = 0x01;
    pub const PUBLIC: u16 = 0x02;
    pub const WEAK: u16 = 0x04;
    /// Stored in 8 bytes: the key in the size field, the value's type in the flags' high byte.
    pub const COMPACT: u16 = 0x08;
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntryValue {
    Simple(Value),
    /// A style, array, plural or attribute: its parent (0 for none) and its items by name.
    Complex {
        parent: u32,
        map: Vec<(u32, Value)>,
    },
}

/// `Res_value`: a type (`TYPE_STRING`, `TYPE_INT_DEC`, ...) and its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub ty: u8,
    pub data: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(utf8: bool, strings: &[&str]) -> StringPool {
        StringPool {
            utf8,
            sorted: false,
            strings: strings.iter().map(|s| s.to_string()).collect(),
            styles: vec![],
        }
    }

    fn simple(flags: u16, key: u32, ty: u8, data: u32) -> Option<Entry> {
        Some(Entry {
            flags,
            key,
            value: EntryValue::Simple(Value { ty, data }),
        })
    }

    /// A table with every layout the writer knows: both pool encodings, styles, a string
    /// outside the BMP, dense, sparse and 16-bit-offset types, compact, weak and complex
    /// entries, and a chunk kept as read.
    fn sample(utf8: bool) -> Table {
        let mut strings = pool(
            utf8,
            &["plain", "bold text", "b", "emoji 😀", &"x".repeat(200)],
        );
        strings.styles = vec![
            vec![],
            vec![Span {
                name: 2,
                first: 0,
                last: 3,
            }],
        ];
        let mut config = vec![0u8; 64];
        config[0] = 64;
        let mut night = config.clone();
        night[29] = 0x20;
        let ty = |flags, config: &Vec<u8>, entries| {
            Chunk::Type(Type {
                id: 1,
                flags,
                reserved: 0,
                config: config.clone(),
                entries,
            })
        };
        Table {
            strings,
            packages: vec![Package {
                id: 0x7f,
                name: "com.example".into(),
                last_public_type: 0,
                last_public_key: 0,
                type_id_offset: 0,
                types: pool(utf8, &["string", "style"]),
                keys: pool(utf8, &["a", "b", "c", "theme"]),
                chunks: vec![
                    Chunk::Spec(Spec {
                        id: 1,
                        res0: 0,
                        res1: 3,
                        flags: vec![0, 0x200, 0],
                    }),
                    ty(
                        0,
                        &config,
                        vec![simple(0, 0, 3, 0), None, simple(Entry::PUBLIC, 2, 3, 1)],
                    ),
                    ty(
                        Type::SPARSE,
                        &night,
                        vec![None, simple(Entry::COMPACT, 1, 3, 3)],
                    ),
                    ty(
                        Type::OFFSET16,
                        &night,
                        vec![simple(Entry::WEAK, 0, 0x10, 7), None, None],
                    ),
                    Chunk::Spec(Spec {
                        id: 2,
                        res0: 0,
                        res1: 1,
                        flags: vec![0],
                    }),
                    Chunk::Type(Type {
                        id: 2,
                        flags: 0,
                        reserved: 0,
                        config: config.clone(),
                        entries: vec![Some(Entry {
                            flags: Entry::COMPLEX,
                            key: 3,
                            value: EntryValue::Complex {
                                parent: 0x01030005,
                                map: vec![(
                                    0x01010036,
                                    Value {
                                        ty: 0x12,
                                        data: 0xffff_ffff,
                                    },
                                )],
                            },
                        })],
                    }),
                    Chunk::Other(vec![0x06, 0x02, 0x0c, 0, 0x0c, 0, 0, 0, 0, 0, 0, 0]),
                ],
            }],
        }
    }

    #[test]
    fn writes_what_it_reads() {
        for utf8 in [true, false] {
            let t = sample(utf8);
            let bytes = write(&t);
            assert_eq!(read(&bytes).unwrap(), t, "utf8: {utf8}");
            assert_eq!(write(&read(&bytes).unwrap()), bytes);
        }
    }

    /// Every resources.arsc in a directory, and every one in an .apk there, must write back
    /// to the same bytes:
    /// APKSCOPE_TABLES=<dir> cargo test --release tables_round_trip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn tables_round_trip() {
        use std::io::Read;
        let dir = std::env::var("APKSCOPE_TABLES").expect("set APKSCOPE_TABLES");
        let mut checked = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let bytes = match path.extension().and_then(|x| x.to_str()) {
                Some("arsc") => std::fs::read(&path).unwrap(),
                Some("apk") => {
                    let mut z = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
                    let Ok(mut e) = z.by_name("resources.arsc") else {
                        continue;
                    };
                    let mut b = vec![];
                    e.read_to_end(&mut b).unwrap();
                    b
                }
                _ => continue,
            };
            let t = read(&bytes).unwrap_or_else(|e| panic!("{}: {}", path.display(), e.report()));
            assert!(
                write(&t) == bytes,
                "{} writes back differently",
                path.display()
            );
            checked += 1;
        }
        println!("{checked} tables written back unchanged");
    }
}
