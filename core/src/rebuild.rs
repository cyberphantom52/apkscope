//! Rebuild an APK with edits: untouched entries are copied byte for byte (no recompression),
//! replaced or added files are written fresh, old v1 signature files are dropped, and
//! uncompressed data is aligned the way `zipalign` does (4 bytes, 4096 for native libraries).

use std::collections::BTreeMap;
use std::io::Read;

/// name -> new content, or None to delete
pub type Edits = BTreeMap<String, Option<Vec<u8>>>;

/// 1981-01-01 00:00, the fixed timestamp Android's build tools use
const DOS_DATE: u16 = (1 << 9) | (1 << 5) | 1;

struct Entry {
    name: String,
    method: u16,
    crc: u32,
    csize: u32,
    usize: u32,
    offset: u32,
}

/// v1 (JAR) signature files: stale once the APK changes, and replaced by v2/v3
pub fn is_v1_signature(name: &str) -> bool {
    let up = name.to_uppercase();
    up.starts_with("META-INF/")
        && !up[9..].contains('/')
        && (up == "META-INF/MANIFEST.MF"
            || [".SF", ".RSA", ".DSA", ".EC"]
                .iter()
                .any(|x| up.ends_with(x)))
}

/// Entries that must stay uncompressed: Android maps them straight from the file.
fn must_store(name: &str) -> bool {
    name == "resources.arsc" || name.ends_with(".so")
}

fn alignment(name: &str, method: u16) -> usize {
    match (method, name.ends_with(".so")) {
        (0, true) => 4096,
        (0, false) => 4,
        _ => 1,
    }
}

fn push16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}

fn push32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}

/// Append one local file header (with alignment padding) and its data.
fn write_entry(
    out: &mut Vec<u8>,
    name: &str,
    method: u16,
    crc: u32,
    usize: u32,
    data: &[u8],
) -> Result<Entry, String> {
    let offset = u32::try_from(out.len()).map_err(|_| "APK larger than 4 GB")?;
    let align = alignment(name, method);
    let base = out.len() + 30 + name.len();
    // pad with Android's alignment extra field (id 0xd935): id, size, alignment, zeros
    let extra = if align > 1 {
        let mut len = 6;
        while !(base + len).is_multiple_of(align) {
            len += 1;
        }
        let mut e = Vec::with_capacity(len);
        push16(&mut e, 0xd935);
        push16(&mut e, (len - 4) as u16);
        push16(&mut e, align as u16);
        e.resize(len, 0);
        e
    } else {
        Vec::new()
    };
    let flags: u16 = if name.is_ascii() { 0 } else { 0x800 };
    push32(out, 0x0403_4b50);
    push16(out, if method == 0 { 10 } else { 20 });
    push16(out, flags);
    push16(out, method);
    push16(out, 0);
    push16(out, DOS_DATE);
    push32(out, crc);
    push32(out, data.len() as u32);
    push32(out, usize);
    push16(out, name.len() as u16);
    push16(out, extra.len() as u16);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&extra);
    out.extend_from_slice(data);
    Ok(Entry {
        name: name.to_string(),
        method,
        crc,
        csize: data.len() as u32,
        usize,
        offset,
    })
}

/// Rebuild `src` with `edits` applied. Returns the unsigned APK and its central directory
/// offset (where a signing block goes).
pub fn rebuild(src: &str, edits: &Edits) -> Result<(Vec<u8>, usize), String> {
    let f = std::fs::File::open(src).map_err(|e| format!("{src}: {e}"))?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("{src}: {e}"))?;
    let mut out = Vec::with_capacity(std::fs::metadata(src).map_or(0, |m| m.len() as usize));
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for i in 0..zip.len() {
        let mut e = zip.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = e.name().map_err(|e| e.to_string())?.into_owned();
        if name.ends_with('/') || is_v1_signature(&name) || !seen.insert(name.clone()) {
            continue;
        }
        match edits.get(&name) {
            Some(None) => continue,
            Some(Some(new)) => {
                drop(e);
                entries.push(fresh(&mut out, &name, new)?);
            }
            None => {
                let method = match e.compression() {
                    zip::CompressionMethod::Stored => 0,
                    zip::CompressionMethod::Deflated => 8,
                    other => return Err(format!("{name}: unsupported compression {other:?}")),
                };
                let (crc, usize) = (e.crc32(), e.size() as u32);
                let mut raw = Vec::with_capacity(e.compressed_size() as usize);
                e.read_to_end(&mut raw)
                    .map_err(|err| format!("{name}: {err}"))?;
                entries.push(write_entry(&mut out, &name, method, crc, usize, &raw)?);
            }
        }
    }
    // added files
    for (name, new) in edits {
        if let Some(new) = new
            && !seen.contains(name)
        {
            entries.push(fresh(&mut out, name, new)?);
        }
    }
    if entries.len() > 0xffff {
        return Err("more than 65535 files: Android can't read ZIP64 APKs".into());
    }
    let cd = out.len();
    for e in &entries {
        push32(&mut out, 0x0201_4b50);
        push16(&mut out, 20);
        push16(&mut out, if e.method == 0 { 10 } else { 20 });
        push16(&mut out, if e.name.is_ascii() { 0 } else { 0x800 });
        push16(&mut out, e.method);
        push16(&mut out, 0);
        push16(&mut out, DOS_DATE);
        push32(&mut out, e.crc);
        push32(&mut out, e.csize);
        push32(&mut out, e.usize);
        push16(&mut out, e.name.len() as u16);
        out.extend_from_slice(&[0; 8]); // extra and comment length, disk, internal attrs
        push32(&mut out, 0); // external attrs
        push32(&mut out, e.offset);
        out.extend_from_slice(e.name.as_bytes());
    }
    let cd_size = out.len() - cd;
    push32(&mut out, 0x0605_4b50);
    push32(&mut out, 0);
    push16(&mut out, entries.len() as u16);
    push16(&mut out, entries.len() as u16);
    push32(&mut out, cd_size as u32);
    push32(
        &mut out,
        u32::try_from(cd).map_err(|_| "APK larger than 4 GB")?,
    );
    push16(&mut out, 0);
    Ok((out, cd))
}

/// A new or replaced file: stored when Android needs it uncompressed, else deflated.
fn fresh(out: &mut Vec<u8>, name: &str, data: &[u8]) -> Result<Entry, String> {
    let crc = crc32fast::hash(data);
    let usize = u32::try_from(data.len()).map_err(|_| format!("{name}: larger than 4 GB"))?;
    if must_store(name) {
        return write_entry(out, name, 0, crc, usize, data);
    }
    let z = miniz_oxide::deflate::compress_to_vec(data, 6);
    // keep it stored when deflate doesn't help (already-compressed media)
    if z.len() >= data.len() {
        write_entry(out, name, 0, crc, usize, data)
    } else {
        write_entry(out, name, 8, crc, usize, &z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> std::path::PathBuf {
        use std::io::Write;
        let p = std::env::temp_dir().join(format!("apkscope-rebuild-{}.apk", std::process::id()));
        let mut w = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("AndroidManifest.xml", deflated).unwrap();
        w.write_all(&[1u8; 300]).unwrap();
        w.start_file("lib/arm64-v8a/libx.so", stored).unwrap();
        w.write_all(b"\x7fELF-not-really").unwrap();
        w.start_file("assets/a.txt", deflated).unwrap();
        w.write_all(b"hello hello hello hello").unwrap();
        w.start_file("META-INF/CERT.RSA", stored).unwrap();
        w.write_all(b"old signature").unwrap();
        w.finish().unwrap();
        p
    }

    #[test]
    fn edits_alignment_and_old_signatures() {
        let src = sample();
        let mut edits = Edits::new();
        edits.insert("assets/a.txt".into(), Some(b"replaced".to_vec()));
        edits.insert("assets/new.bin".into(), Some(vec![7; 10]));
        edits.insert("AndroidManifest.xml".into(), None);
        let (bytes, cd) = rebuild(src.to_str().unwrap(), &edits).unwrap();
        let out = std::env::temp_dir().join(format!("apkscope-rebuilt-{}.apk", std::process::id()));
        std::fs::write(&out, &bytes).unwrap();
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = z
            .file_names()
            .filter_map(|n| n.ok().map(|n| n.into_owned()))
            .collect();
        assert_eq!(
            names,
            vec!["lib/arm64-v8a/libx.so", "assets/a.txt", "assets/new.bin"]
        );
        let mut s = String::new();
        z.by_name("assets/a.txt")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "replaced");
        // the .so is stored and page-aligned, so Android can map it in place
        let so = z.by_name("lib/arm64-v8a/libx.so").unwrap();
        assert_eq!(so.data_start().unwrap() % 4096, 0);
        assert_eq!(
            u32::from_le_bytes(bytes[bytes.len() - 6..bytes.len() - 2].try_into().unwrap())
                as usize,
            cd
        );
        let _ = std::fs::remove_file(src);
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn v1_signature_names() {
        assert!(
            is_v1_signature("META-INF/CERT.RSA")
                && is_v1_signature("META-INF/MANIFEST.MF")
                && is_v1_signature("META-INF/x.SF")
        );
        assert!(
            !is_v1_signature("META-INF/services/foo")
                && !is_v1_signature("META-INF/kotlin.kotlin_module")
        );
    }
}
