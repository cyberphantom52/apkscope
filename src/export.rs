//! `apkscope export`: APK -> Gradle project (sources + resources).

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::dex::{self, Dex};
use crate::java;

pub struct Apk {
    pub zip: zip::ZipArchive<fs::File>,
    /// raw bytes of root-level classes*.dex, in load order
    pub dex: Vec<(String, Vec<u8>)>,
}

pub fn open(path: &str) -> Result<Apk, String> {
    let file = fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{path}: {e}"))?;
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

impl Apk {
    pub fn read(&mut self, name: &str) -> Option<Vec<u8>> {
        let mut e = self.zip.by_name(name).ok()?;
        let mut buf = Vec::with_capacity(e.size() as usize);
        e.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3",
];

/// Unique, filesystem-safe path for a class. Obfuscated `a` and `A` collide on Windows/macOS,
/// so names are de-duplicated case-insensitively.
fn class_path(desc: &str, taken: &Mutex<HashSet<String>>) -> PathBuf {
    let inner = desc
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(desc);
    let safe: Vec<String> = inner
        .split('/')
        .map(|p| {
            let p: String = p
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || "$_-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if RESERVED.contains(&p.to_lowercase().as_str()) {
                format!("{p}_")
            } else {
                p
            }
        })
        .collect();
    let base = safe.join("/");
    let mut taken = taken.lock().unwrap();
    let mut path = base.clone();
    let mut n = 1;
    while !taken.insert(path.to_lowercase()) {
        n += 1;
        path = format!("{base}_{n}");
    }
    PathBuf::from(path + ".java")
}

/// Decompile every class into `src_dir`, in parallel. Returns (classes written, classes failed).
pub fn sources(apk: &Apk, src_dir: &Path, deobf: bool) -> (usize, usize) {
    let dexes: Vec<Dex> = apk
        .dex
        .iter()
        .filter_map(|(n, b)| Dex::parse(b).map_err(|e| eprintln!("{n}: {e}")).ok())
        .collect();
    if deobf {
        crate::deobf::init(&dexes);
    }
    crate::imports::init(&dexes);
    // (dex, class) jobs; a class defined twice keeps its first definition
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for (d, dex) in dexes.iter().enumerate() {
        let nest = dex.nesting();
        for (c, parent) in nest.parent.iter().enumerate() {
            // inner classes are written inside their outer class's file
            if parent.is_some() {
                continue;
            }
            if let Ok(desc) = dex.class_descriptor(c)
                && seen.insert(desc)
            {
                jobs.push((d, c));
            }
        }
    }
    drop(seen);
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let taken = Mutex::new(HashSet::new());
    // one core stays free for the writer thread
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .saturating_sub(1)
        .max(1);
    // decompile on all cores, write from one thread: parallel file creation is slower on NTFS
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, String)>(256);
    std::thread::scope(|s| {
        let failed_w = &failed;
        s.spawn(move || {
            let mut made = HashSet::new();
            for (path, text) in rx {
                if let Some(dir) = path.parent()
                    && made.insert(dir.to_path_buf())
                {
                    let _ = fs::create_dir_all(dir);
                }
                if let Err(e) = fs::write(&path, text) {
                    eprintln!("{}: {e}", path.display());
                    failed_w.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        for _ in 0..threads {
            let tx = tx.clone();
            let (next, jobs, dexes, failed, taken) = (&next, &jobs, &dexes, &failed, &taken);
            std::thread::Builder::new()
                // structuring recurses per nesting level
                .stack_size(64 << 20)
                .spawn_scoped(s, move || {
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(d, c)) = jobs.get(k) else { break };
                        let dex = &dexes[d];
                        let Ok(desc) = dex.class_descriptor(c) else {
                            continue;
                        };
                        let text = java::class(dex, c).unwrap_or_else(|e| {
                            failed.fetch_add(1, Ordering::Relaxed);
                            format!(
                                "/* class {} failed: {e} */
",
                                dex::java_name(&desc)
                            )
                        });
                        let desc = crate::deobf::class(&desc).unwrap_or(&desc);
                        let _ = tx.send((src_dir.join(class_path(desc, taken)), text));
                    }
                })
                .unwrap();
        }
        drop(tx);
    });
    (jobs.len(), failed.into_inner())
}

#[derive(Default)]
pub struct ResStats {
    pub values: usize,
    pub files: usize,
    pub xml_decoded: usize,
    pub other: usize,
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

/// Manifest, res/ (values + files with their real names), assets/ and native libs into `main_dir`.
pub fn resources(apk: &mut Apk, main_dir: &Path) -> Result<(ResStats, Option<Manifest>), String> {
    let mut st = ResStats::default();
    let table = apk
        .read("resources.arsc")
        .map(|d| crate::res::parse_table(&d))
        .transpose()?;
    let names = table.as_ref().map(crate::res::Names::new);
    let mut manifest = None;
    if let Some(m) = apk.read("AndroidManifest.xml") {
        let text = crate::res::decode_xml(&m, names.as_ref())?;
        manifest = Some(Manifest::from_xml(&text));
        write(&main_dir.join("AndroidManifest.xml"), text.as_bytes())?;
    }
    let mut used = HashSet::new();
    if let (Some(t), Some(n)) = (&table, &names) {
        let dec = crate::res::decode_table(t, n);
        for (path, text) in &dec.values {
            write(&main_dir.join("res").join(path), text.as_bytes())?;
            st.values += 1;
        }
        for (path, src) in &dec.files {
            let Some(data) = apk.read(src) else { continue };
            used.insert(src.clone());
            let out = main_dir.join("res").join(path);
            match crate::res::decode_xml(&data, Some(n)) {
                Ok(text) => {
                    write(&out, text.as_bytes())?;
                    st.xml_decoded += 1;
                }
                Err(_) => write(&out, &data)?,
            }
            st.files += 1;
        }
    }
    // assets and native libraries are copied as-is
    let names_in_zip: Vec<String> = apk
        .zip
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .collect();
    for name in names_in_zip {
        let dest = if let Some(rest) = name.strip_prefix("assets/") {
            main_dir.join("assets").join(rest)
        } else if let Some(rest) = name.strip_prefix("lib/") {
            main_dir.join("jniLibs").join(rest)
        } else {
            continue;
        };
        if name.ends_with('/') || name.contains("..") {
            continue; // directory entries; never write outside the output folder
        }
        if let Some(data) = apk.read(&name) {
            write(&dest, &data)?;
            st.other += 1;
        }
    }
    Ok((st, manifest))
}

#[derive(Default, Debug, PartialEq)]
pub struct Manifest {
    pub package: String,
    pub min_sdk: String,
    pub target_sdk: String,
    pub version_code: String,
    pub version_name: String,
}

impl Manifest {
    fn from_xml(text: &str) -> Manifest {
        // attribute values from the decoded text; first occurrence wins
        let attr = |key: &str| -> String {
            text.find(&format!("{key}=\"")).map_or(String::new(), |i| {
                let rest = &text[i + key.len() + 2..];
                rest[..rest.find('"').unwrap_or(0)].to_string()
            })
        };
        Manifest {
            package: attr("package"),
            min_sdk: attr("android:minSdkVersion"),
            target_sdk: attr("android:targetSdkVersion"),
            version_code: attr("android:versionCode"),
            version_name: attr("android:versionName"),
        }
    }
}

/// settings.gradle, build.gradle and app/build.gradle for Android Studio.
pub fn gradle(root: &Path, m: &Manifest) -> Result<(), String> {
    let or = |v: &str, d: &str| {
        if v.is_empty() {
            d.to_string()
        } else {
            v.to_string()
        }
    };
    let name = m
        .package
        .rsplit('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("app");
    write(
        &root.join("settings.gradle"),
        format!(
            "rootProject.name = '{name}'
include ':app'
"
        )
        .as_bytes(),
    )?;
    write(
        &root.join("build.gradle"),
        b"// Decompiled by apkscope. Sources are reconstructed and may not compile as-is.
plugins {
    id 'com.android.application' version '8.5.0' apply false
}
",
    )?;
    let target = or(&m.target_sdk, "34");
    let app = format!(
        "plugins {{
    id 'com.android.application'
}}

android {{
    namespace '{pkg}'
    compileSdk {target}

    defaultConfig {{
        applicationId '{pkg}'
        minSdk {min}
        targetSdk {target}
        versionCode {code}
        versionName '{vname}'
    }}
}}
",
        pkg = or(&m.package, "app"),
        min = or(&m.min_sdk, "21"),
        code = or(&m.version_code, "1"),
        vname = or(&m.version_name, "1.0"),
    );
    write(&root.join("app/build.gradle"), app.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_paths_are_unique_and_safe() {
        let taken = Mutex::new(HashSet::new());
        assert_eq!(
            class_path("Lcom/a/B;", &taken),
            PathBuf::from("com/a/B.java")
        );
        assert_eq!(
            class_path("Lcom/a/b;", &taken),
            PathBuf::from("com/a/b_2.java")
        );
        assert_eq!(
            class_path("Lcon/Aux;", &taken),
            PathBuf::from("con_/Aux_.java")
        );
    }

    #[test]
    fn manifest_fields() {
        let m = Manifest::from_xml(
            "<manifest package=\"a.b\" android:versionCode=\"7\">
<uses-sdk android:minSdkVersion=\"24\" android:targetSdkVersion=\"34\" />",
        );
        assert_eq!(
            (
                m.package.as_str(),
                m.min_sdk.as_str(),
                m.target_sdk.as_str(),
                m.version_code.as_str()
            ),
            ("a.b", "24", "34", "7")
        );
    }
}
