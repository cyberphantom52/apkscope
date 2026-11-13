//! An APK (or a JAR of DEX files) as Android loads it, from a file or from anything that reads
//! and seeks, such as a `Cursor` over bytes already in memory.

use std::fs;
use std::io::{Read, Seek};

pub struct Apk<R = fs::File> {
    pub zip: zip::ZipArchive<R>,
    /// raw bytes of root-level classes*.dex, in load order
    pub dex: Vec<(String, Vec<u8>)>,
}

pub fn open(path: &str) -> Result<Apk, String> {
    let file = fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    Apk::new(file).map_err(|e| format!("{path}: {e}"))
}

impl<R: Read + Seek> Apk<R> {
    pub fn new(reader: R) -> Result<Self, String> {
        let mut zip = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
        let mut dex = Vec::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = entry.name().map_err(|e| e.to_string())?.into_owned();
            // only root-level classes*.dex are loaded by Android
            if name.starts_with("classes") && name.ends_with(".dex") && !name.contains('/') {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("{name}: {e}"))?;
                dex.push((name, buf));
            }
        }
        // classes.dex, classes2.dex, ... (first definition of a class wins at runtime)
        dex.sort_by_key(|(n, _)| {
            n.trim_start_matches("classes")
                .trim_end_matches(".dex")
                .parse::<u32>()
                .unwrap_or(1)
        });
        Ok(Apk { zip, dex })
    }

    pub fn read(&mut self, name: &str) -> Option<Vec<u8>> {
        let mut e = self.zip.by_name(name).ok()?;
        let mut buf = Vec::with_capacity(e.size() as usize);
        e.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}
