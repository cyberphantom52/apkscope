//! One opened APK and everything that searches it: the reference index, the decompiled-code
//! cache, scoped searches, usages, result export and the manifest overview. Shared by the
//! viewer and the `search` command.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::dex::{self, Dex};
use crate::export::{self, Apk};
use crate::index::{self, Index};
use crate::{bytecode, cache, elf, java, res};

/// Worker threads for indexing and searching (0 = all cores).
pub static THREADS: AtomicUsize = AtomicUsize::new(0);

/// Whether a full scan may write the decompiled-code cache.
pub static CACHE_WRITE: AtomicBool = AtomicBool::new(true);

pub fn threads() -> usize {
    match THREADS.load(Ordering::Relaxed) {
        0 => std::thread::available_parallelism().map_or(4, |n| n.get()),
        n => n,
    }
}

/// Resident memory of this process, in bytes.
#[cfg(feature = "gui")]
pub fn memory_used() -> Option<u64> {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            faults: u32,
            peak_ws: usize,
            ws: usize,
            a: usize,
            b: usize,
            c: usize,
            d: usize,
            pagefile: usize,
            peak_pagefile: usize,
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(p: isize, c: *mut Counters, cb: u32) -> i32;
        }
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            ..Default::default()
        };
        // safety: c is a correctly sized PROCESS_MEMORY_COUNTERS that outlives the call
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (ok != 0).then_some(c.ws as u64)
    }
    #[cfg(not(windows))]
    {
        let s = std::fs::read_to_string("/proc/self/statm").ok()?;
        s.split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|p| p * 4096)
    }
}

/// Where a top-level class comes from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Origin {
    /// the app's own package
    App,
    /// unknown or obfuscated packages: may well be app code
    Other,
    /// a known SDK or library
    Library,
}

/// Package prefixes of common Android SDKs and libraries (Java names, dot-terminated).
const LIBRARIES: &[&str] = &[
    "android.",
    "androidx.",
    "com.android.",
    "dalvik.",
    "java.",
    "javax.",
    "kotlin.",
    "kotlinx.",
    "org.jetbrains.",
    "org.intellij.",
    "com.google.",
    "com.squareup.",
    "okhttp3.",
    "okio.",
    "retrofit2.",
    "io.reactivex.",
    "rx.",
    "io.grpc.",
    "com.facebook.",
    "com.bumptech.",
    "dagger.",
    "hilt_aggregated_deps.",
    "com.fasterxml.",
    "org.json.",
    "org.apache.",
    "org.slf4j.",
    "ch.qos.",
    "com.airbnb.",
    "io.sentry.",
    "com.crashlytics.",
    "io.fabric.",
    "com.adjust.",
    "com.appsflyer.",
    "com.unity3d.",
    "com.applovin.",
    "com.ironsource.",
    "com.mopub.",
    "com.chartboost.",
    "com.vungle.",
    "com.inmobi.",
    "com.amazon.",
    "com.microsoft.",
    "org.chromium.",
    "io.flutter.",
    "com.facebook.react.",
    "org.greenrobot.",
    "net.sqlcipher.",
    "org.bouncycastle.",
    "org.conscrypt.",
    "com.tencent.",
    "com.huawei.",
    "io.branch.",
    "com.onesignal.",
    "com.revenuecat.",
    "io.realm.",
    "org.koin.",
    "coil.",
    "com.github.",
    "io.ktor.",
    "org.reactivestreams.",
    "com.jakewharton.",
    "timber.",
    "io.opentelemetry.",
    "com.braze.",
    "com.mixpanel.",
    "com.segment.",
    "com.stripe.",
    "com.paypal.",
    "com.yandex.",
    "com.bytedance.sdk.",
];

/// Origin of a class by its (displayed) Java name. App code is the manifest package's
/// organization (`com.unciv.app` -> `com.unciv`), since apps often keep code beside the
/// launcher package; known libraries are checked first.
pub fn origin(name: &str, package: &str) -> Origin {
    if LIBRARIES.iter().any(|p| name.starts_with(p)) {
        return Origin::Library;
    }
    let parts: Vec<&str> = package.split('.').collect();
    let root = if parts.len() >= 3 {
        parts[..2].join(".")
    } else {
        package.to_string()
    };
    if !root.is_empty() && name.starts_with(&root) && name[root.len()..].starts_with('.') {
        Origin::App
    } else {
        Origin::Other
    }
}

pub struct State {
    pub path: String,
    /// the APK actually read (differs from `path` for bundles and .dex)
    pub file: String,
    pub dexes: Vec<Dex<'static>>,
    /// top-level classes: (java name, dex, class index); the position is the class id
    pub classes: Vec<(String, usize, usize)>,
    /// res path -> text (manifest, values XML) decoded at startup
    pub texts: BTreeMap<String, String>,
    /// res path -> path inside the APK, decoded on demand
    pub files: BTreeMap<String, String>,
    names: Option<res::Names>,
    apk: Mutex<Apk>,
    /// every zip entry name, for the overview
    pub entries: Vec<String>,
    pub package: String,
    pub methods: usize,
    /// origin of each top-level class (same order as `classes`)
    pub origin: Vec<Origin>,
    natives: OnceLock<Vec<(String, String)>>,
    pub index: OnceLock<Index>,
    pub indexed: AtomicUsize,
    cache: RwLock<Option<Arc<cache::Reader>>>,
    cache_dir: std::path::PathBuf,
}

/// What apkscope opens, for file dialogs and the start screen.
pub const INPUTS: [&str; 5] = ["apk", "jar", "apks", "xapk", "dex"];

/// Turn any supported input into an APK path: a `.jar` of DEX files (Android's framework JARs)
/// reads as one, split bundles (`.apks`, `.xapk`) give their `base.apk`, a bare `.dex` is
/// wrapped in a minimal APK. Both land in the cache folder.
pub fn prepare(path: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let ext = std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let stem = std::path::Path::new(path)
        .file_stem()
        .map_or("input".into(), |s| s.to_string_lossy().into_owned());
    let dir = cache::root().join("inputs");
    match ext.as_str() {
        "apk" | "jar" => Ok(path.to_string()),
        "apks" | "xapk" => {
            let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
            let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("{path}: {e}"))?;
            let names: Vec<String> = zip
                .file_names()
                .filter_map(|n| n.ok().map(|n| n.into_owned()))
                .filter(|n| n.ends_with(".apk"))
                .collect();
            // the base split holds the code; fall back to the largest APK inside
            let base = names
                .iter()
                .find(|n| n.rsplit('/').next() == Some("base.apk"))
                .cloned()
                .or_else(|| {
                    names
                        .iter()
                        .max_by_key(|n| zip.by_name(n).map(|e| e.size()).unwrap_or(0))
                        .cloned()
                });
            let base = base.ok_or("no APK inside this bundle")?;
            let size = zip.by_name(&base).map_err(|e| e.to_string())?.size();
            let out = dir.join(format!("{stem}-{size:x}-base.apk"));
            if !out.exists() {
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let mut data = Vec::new();
                zip.by_name(&base)
                    .map_err(|e| e.to_string())?
                    .read_to_end(&mut data)
                    .map_err(|e| e.to_string())?;
                std::fs::write(&out, data).map_err(|e| e.to_string())?;
            }
            Ok(out.display().to_string())
        }
        "dex" => {
            let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            Dex::parse(&data).map_err(|e| format!("{path}: {e}"))?;
            let out = dir.join(format!("{stem}-{:x}.apk", data.len()));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let mut w =
                zip::ZipWriter::new(std::fs::File::create(&out).map_err(|e| e.to_string())?);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("classes.dex", opts)
                .map_err(|e| e.to_string())?;
            w.write_all(&data).map_err(|e| e.to_string())?;
            w.finish().map_err(|e| e.to_string())?;
            Ok(out.display().to_string())
        }
        _ => Err(format!(
            "apkscope opens {}; not .{ext}",
            INPUTS.map(|e| format!(".{e}")).join(", ")
        )),
    }
}

pub fn load(path: &str, deobf: bool) -> Result<State, String> {
    let real = prepare(path)?;
    let mut apk = export::open(&real)?;
    // a session lives until the process exits, so the DEX bytes can be 'static
    let raw: Vec<&'static [u8]> = std::mem::take(&mut apk.dex)
        .into_iter()
        .map(|(_, b)| &*Box::leak(b.into_boxed_slice()))
        .collect();
    let dexes: Vec<Dex<'static>> = raw.iter().filter_map(|b| Dex::parse(b).ok()).collect();
    if deobf {
        crate::deobf::init(&dexes);
    }
    let notes = crate::re::load_notes(path)?;
    let tag = crate::re::notes_tag(&notes);
    crate::deobf::set_notes(notes);
    // inline string decryption needs every DEX, from any thread
    crate::re::set_dexes(Box::leak(
        raw.iter()
            .filter_map(|b| Dex::parse(b).ok())
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    ));
    crate::imports::init(&dexes);
    let mut classes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (d, dx) in dexes.iter().enumerate() {
        let nest = dx.nesting();
        for (c, parent) in nest.parent.iter().enumerate() {
            if parent.is_none()
                && let Ok(desc) = dx.class_descriptor(c)
                && seen.insert(desc.clone())
            {
                classes.push((dex::java_name(&desc), d, c));
            }
        }
    }
    classes.sort();
    let entries: Vec<String> = apk
        .zip
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .collect();
    let table = apk
        .read("resources.arsc")
        .and_then(|d| apkscope_core::table::read(&d).ok());
    let names = table.as_ref().map(res::Names::new);
    let mut texts = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut package = String::new();
    if let Some(m) = apk.read("AndroidManifest.xml")
        && let Ok(t) = res::decode_xml(&m, names.as_ref())
    {
        package = attr(&t, "package").unwrap_or_default();
        texts.insert("AndroidManifest.xml".to_string(), t);
    }
    if let (Some(t), Some(n)) = (&table, &names) {
        let dec = res::decode_table(t, n);
        for (p, text) in dec.values {
            texts.insert(format!("res/{p}"), text);
        }
        for (p, src) in dec.files {
            files.insert(format!("res/{p}"), src);
        }
    }
    let methods = dexes
        .iter()
        .map(|d| {
            (0..d.class_count)
                .map(|c| d.class_data(c).map_or(0, |x| x.methods.len()))
                .sum::<usize>()
        })
        .sum();
    let origin = classes
        .iter()
        .map(|c| self::origin(&c.0, &package))
        .collect();
    let cache_dir = cache::dir_for(&real, deobf, tag);
    let reader = cache::Reader::open(&cache_dir, classes.len()).map(Arc::new);
    Ok(State {
        path: path.to_string(),
        file: real.clone(),
        dexes,
        classes,
        texts,
        files,
        names,
        apk: Mutex::new(apk),
        entries,
        package,
        methods,
        origin,
        natives: OnceLock::new(),
        index: OnceLock::new(),
        indexed: AtomicUsize::new(0),
        cache: RwLock::new(reader),
        cache_dir,
    })
}

impl State {
    /// Build the reference index (call once, off the UI thread).
    pub fn build_index(&self) {
        let list: Vec<(usize, usize)> = self.classes.iter().map(|c| (c.1, c.2)).collect();
        let idx = index::build(&self.dexes, &list, threads(), &self.indexed);
        let _ = self.index.set(idx);
    }

    pub fn cached(&self) -> bool {
        self.cache.read().unwrap().is_some()
    }

    /// Top-level class by its shown name, or by its original (obfuscated) name or descriptor.
    pub fn find(&self, name: &str) -> Option<usize> {
        let by = |n: &str| self.classes.binary_search_by(|c| c.0.as_str().cmp(n)).ok();
        by(name).or_else(|| {
            let desc = if name.starts_with('L') && name.ends_with(';') {
                name.to_string()
            } else {
                format!("L{};", name.replace('.', "/"))
            };
            by(&dex::java_name(&desc))
        })
    }

    /// Java of top-level class `k`, from the cache when there is one.
    fn java_of(
        &self,
        k: usize,
        reader: Option<(&cache::Reader, &mut std::fs::File)>,
    ) -> Option<String> {
        if let Some((r, f)) = reader
            && let Some(t) = r.get(f, k)
        {
            return Some(t);
        }
        let (_, d, c) = &self.classes[k];
        java::class(&self.dexes[*d], *c).ok()
    }

    pub fn class_text(&self, name: &str, smali: bool) -> Option<String> {
        let k = self.find(name)?;
        let (_, d, c) = &self.classes[k];
        let dx = &self.dexes[*d];
        if !smali {
            let reader = self.cache.read().unwrap().clone();
            let mut f = reader.as_ref().and_then(|r| r.file());
            let text = match (&reader, f.as_mut()) {
                (Some(r), Some(f)) => self.java_of(k, Some((r, f))),
                _ => None,
            };
            return Some(text.unwrap_or_else(|| {
                java::class(dx, *c).unwrap_or_else(|e| format!("/* failed: {e} */"))
            }));
        }
        Some(self::smali(dx, *c))
    }

    /// Text of a resource; binary files are described, not shown.
    pub fn resource(&self, path: &str) -> Option<String> {
        if let Some(t) = self.texts.get(path) {
            return Some(t.clone());
        }
        // only paths from the table can be read, so nothing can reach other zip entries
        let src = self.files.get(path)?;
        let data = self.apk.lock().ok()?.read(src)?;
        match res::decode_xml(&data, self.names.as_ref()) {
            Ok(t) => Some(t),
            Err(_) => Some(format!("// binary file, {} bytes ({src})", data.len())),
        }
    }

    pub fn is_res(&self, t: &str) -> bool {
        self.texts.contains_key(t) || self.files.contains_key(t)
    }

    pub fn is_native(&self, t: &str) -> bool {
        t.ends_with(".so") && self.entries.iter().any(|e| e == t)
    }

    /// Text of a hit's target: decompiled class, resource or native-library report.
    pub fn target_text(&self, t: &str) -> Option<String> {
        if self.is_native(t) {
            Some(self.native_report(t))
        } else if self.is_res(t) {
            self.resource(t)
        } else {
            self.class_text(t, false)
        }
    }

    /// Raw bytes of any file inside the APK (only names from its own listing).
    /// Edited manifest text -> binary XML, typed like the APK's own manifest.
    #[cfg(feature = "gui")]
    pub fn encode_manifest(&self, text: &str) -> Result<Vec<u8>, String> {
        let names = self
            .names
            .as_ref()
            .ok_or("this APK has no resources.arsc to resolve @references")?;
        Ok(crate::axml::encode(
            text,
            self.read_entry("AndroidManifest.xml").as_deref(),
            names,
        )?)
    }

    pub fn read_entry(&self, name: &str) -> Option<Vec<u8>> {
        if !self.entries.iter().any(|e| e == name) {
            return None;
        }
        self.apk.lock().ok()?.read(name)
    }

    /// `minSdkVersion` from the manifest.
    #[cfg(feature = "gui")]
    pub fn min_sdk(&self) -> Option<u32> {
        let m = self.texts.get("AndroidManifest.xml")?;
        let at = m.find("<uses-sdk")?;
        attr(&m[at..], "android:minSdkVersion")?.parse().ok()
    }

    /// Every file in the APK as (name, size, compressed size).
    #[cfg(feature = "gui")]
    pub fn files_list(&self) -> Vec<(String, u64, u64)> {
        let mut apk = self.apk.lock().unwrap();
        let mut v: Vec<(String, u64, u64)> = self
            .entries
            .iter()
            .filter(|e| !e.ends_with('/'))
            .map(|e| {
                let (s, c) = apk
                    .zip
                    .by_name(e)
                    .map_or((0, 0), |f| (f.size(), f.compressed_size()));
                (e.clone(), s, c)
            })
            .collect();
        v.sort();
        v
    }

    /// A file inside the APK shown as text: its content when it is text, else a hex dump.
    #[cfg(feature = "gui")]
    pub fn file_view(&self, name: &str) -> String {
        let Some(d) = self.read_entry(name) else {
            return format!("// {name}: not found");
        };
        let mut o = format!(
            "// {name}
// {} bytes

",
            d.len()
        );
        if let Ok(t) = res::decode_xml(&d, self.names.as_ref()) {
            return o + &t;
        }
        match std::str::from_utf8(&d) {
            Ok(t) if d.len() < 4 << 20 && !t.contains('\0') => o + t,
            _ => {
                for (k, row) in d.chunks(16).take(256).enumerate() {
                    let hexs: String = row.iter().map(|b| format!("{b:02x} ")).collect();
                    let text: String = row
                        .iter()
                        .map(|&b| {
                            if (0x20..0x7f).contains(&b) {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    let _ = writeln!(o, "{:08x}  {hexs:<48} {text}", k * 16);
                }
                if d.len() > 4096 {
                    let _ = writeln!(
                        o,
                        "
// first 4096 bytes shown; extract the file for the rest"
                    );
                }
                o
            }
        }
    }

    /// Write every file of the APK under `dir`, skipping names that would escape it.
    #[cfg(feature = "gui")]
    pub fn extract_all(&self, dir: &std::path::Path) -> Result<usize, String> {
        let mut n = 0;
        for (name, _, _) in self.files_list() {
            let rel = std::path::Path::new(&name);
            if name.contains("..") || rel.is_absolute() || name.contains(':') {
                continue;
            }
            let out = dir.join(rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            if let Some(d) = self.read_entry(&name) {
                std::fs::write(&out, d).map_err(|e| format!("{}: {e}", out.display()))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Native libraries as (path, uncompressed size), grouped by ABI folder.
    pub fn native_libs(&self) -> Vec<(String, u64)> {
        let mut apk = self.apk.lock().unwrap();
        let mut v: Vec<(String, u64)> = self
            .entries
            .iter()
            .filter(|e| e.starts_with("lib/") && e.ends_with(".so"))
            .map(|e| (e.clone(), apk.zip.by_name(e).map_or(0, |f| f.size())))
            .collect();
        v.sort();
        v
    }

    /// Java methods declared `native`, as (class, method) with their original (JNI) names.
    pub fn native_methods(&self) -> &[(String, String)] {
        self.natives.get_or_init(|| {
            let mut v = Vec::new();
            for dx in &self.dexes {
                for c in 0..dx.class_count {
                    let Ok(data) = dx.class_data(c) else { continue };
                    for m in data.methods.iter().filter(|m| m.access & 0x100 != 0) {
                        if let (Ok(desc), Ok((_, name, _))) =
                            (dx.class_descriptor(c), dx.method_parts(m.method_idx))
                        {
                            v.push((
                                desc.trim_start_matches('L')
                                    .trim_end_matches(';')
                                    .replace(['/', '$'], "."),
                                name,
                            ));
                        }
                    }
                }
            }
            v.sort();
            v.dedup();
            v
        })
    }

    /// Readable report of one native library.
    pub fn native_report(&self, path: &str) -> String {
        let Some(data) = self.read_entry(path) else {
            return format!("// {path}: not found");
        };
        let mut o = String::new();
        let _ = writeln!(
            o,
            "// {path}
"
        );
        let e = match elf::parse(&data) {
            Ok(e) => e,
            Err(err) => return format!("{o}// {err}, {} bytes", data.len()),
        };
        let _ = writeln!(o, "size              {} bytes", data.len());
        let _ = writeln!(o, "architecture      {} ({}-bit)", e.machine, e.bits);
        let _ = writeln!(
            o,
            "JNI_OnLoad        {}",
            if e.exports.iter().any(|x| x == "JNI_OnLoad") {
                "yes: natives may also be registered at runtime (RegisterNatives), which can't be mapped statically"
            } else {
                "no"
            }
        );
        let _ = writeln!(
            o,
            "
// needs ({})",
            e.needed.len()
        );
        for n in &e.needed {
            let _ = writeln!(o, "{n}");
        }
        let natives = self.native_methods();
        let jni: Vec<(&String, (String, String))> = e
            .exports
            .iter()
            .filter_map(|x| elf::jni_target(x).map(|t| (x, t)))
            .collect();
        let _ = writeln!(
            o,
            "
// JNI exports -> Java native methods ({})",
            jni.len()
        );
        for (sym, (cls, m)) in &jni {
            let found = natives.binary_search(&(cls.clone(), m.clone())).is_ok();
            let _ = writeln!(
                o,
                "{cls}.{m}(){}    <- {sym}",
                if found {
                    ""
                } else {
                    "    (no such native method in the DEX)"
                }
            );
        }
        let _ = writeln!(
            o,
            "
// other exports ({})",
            e.exports.len() - jni.len()
        );
        for x in e
            .exports
            .iter()
            .filter(|x| !x.starts_with("Java_"))
            .take(2000)
        {
            let _ = writeln!(o, "{x}");
        }
        let all = elf::strings(&data, 6);
        let url = regex::Regex::new(URL_RE).unwrap();
        let secret = regex::Regex::new(SECRET_RE).unwrap();
        let interesting: Vec<&String> = all
            .iter()
            .filter(|s| {
                url.is_match(&format!("\"{s}\""))
                    || secret.is_match(&format!("\"{s}\""))
                    || secret.is_match(s)
            })
            .collect();
        let _ = writeln!(
            o,
            "
// URL- or key-like strings ({}). Heuristic: encrypted or computed values won't show",
            interesting.len()
        );
        for s in &interesting {
            let _ = writeln!(o, "{s}");
        }
        let _ = writeln!(
            o,
            "
// all strings, 6+ characters ({}{})",
            all.len().min(5000),
            if all.len() > 5000 { ", first 5000" } else { "" }
        );
        for s in all.iter().take(5000) {
            let _ = writeln!(o, "{s}");
        }
        o
    }

    /// Forget this APK's cache (after the user cleared the cache folder).
    pub fn drop_cache(&self) {
        *self.cache.write().unwrap() = None;
    }

    /// Strings of the app containing `needle` (case-insensitive), with how many classes use each.
    #[cfg(feature = "gui")]
    pub fn strings(&self, needle: &str, limit: usize) -> Vec<(String, usize)> {
        let low = needle.to_lowercase();
        let idx = self.index.get();
        let mut out = Vec::new();
        for (d, dx) in self.dexes.iter().enumerate() {
            for id in 0..dx.counts()[0] {
                let Ok(s) = dx.string(id as u32) else {
                    continue;
                };
                if !s.trim().is_empty() && s.to_lowercase().contains(&low) {
                    let users = idx.map_or(0, |i| i.users(d, index::STRING, id).len());
                    out.push((s, users));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
}

pub const LIMIT: usize = 5000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    Class,
    Code,
    String,
    Usages,
    Res,
    Urls,
    Http,
    Secrets,
}

impl Scope {
    #[cfg(feature = "gui")]
    pub const ALL: [Scope; 8] = [
        Scope::Code,
        Scope::String,
        Scope::Usages,
        Scope::Class,
        Scope::Res,
        Scope::Urls,
        Scope::Http,
        Scope::Secrets,
    ];

    #[cfg(feature = "gui")]
    pub fn label(self) -> &'static str {
        match self {
            Scope::Class => "Class names",
            Scope::Code => "Code",
            Scope::String => "String literals",
            Scope::Usages => "Usages (xref)",
            Scope::Res => "Resources",
            Scope::Urls => "URLs & endpoints",
            Scope::Http => "HTTP / API calls",
            Scope::Secrets => "Keys & secrets",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        Some(match s {
            "class" => Scope::Class,
            "code" => Scope::Code,
            "string" => Scope::String,
            "usages" | "xref" => Scope::Usages,
            "res" => Scope::Res,
            "urls" => Scope::Urls,
            "http" => Scope::Http,
            "secrets" => Scope::Secrets,
            _ => return None,
        })
    }

    /// API scopes list everything they find; the query only narrows them
    #[cfg(feature = "gui")]
    pub fn query_optional(self) -> bool {
        matches!(self, Scope::Urls | Scope::Http | Scope::Secrets)
    }

    /// scopes that read decompiled classes
    pub fn decompiles(self) -> bool {
        !matches!(self, Scope::Class | Scope::Res)
    }

    /// What the scope can miss: shown next to the results.
    pub fn note(self) -> &'static str {
        match self {
            Scope::Code => {
                "Searches decompiled Java. Names are shown after deobfuscation, so search for what you see in the viewer."
            }
            Scope::String => {
                "Matches string literals as stored in the app. Strings built at runtime, encrypted or loaded from native code won't appear."
            }
            Scope::Usages => {
                "Finds classes that reference a method, field, class or string with this name, then the lines that mention it. Calls made via reflection, JNI or dynamic loading are not visible."
            }
            Scope::Class => "Matches class names (deobfuscated).",
            Scope::Res => {
                "Searches decoded XML resources and the manifest, plus resource file names."
            }
            Scope::Urls => {
                "Heuristic: finds URL-like string literals. Endpoints built at runtime, encrypted, downloaded from a config, or kept in native libraries won't be found, and some hits are documentation or ad URLs."
            }
            Scope::Http => {
                "Heuristic: finds common networking APIs (Retrofit, OkHttp, HttpURLConnection). Apps using other libraries, their own wrappers, native code or obfuscated networking may not show up, and library code shows up too."
            }
            Scope::Secrets => {
                "Heuristic: finds well-known key formats and key-like names. A hit is not proof of a leaked secret (many keys are meant to be public), and keys that are encrypted, split or fetched at runtime won't be found."
            }
        }
    }
}

const URL_RE: &str = r#""((?:https?|wss?)://[^"\s]+|/(?:api|v\d+|rest|graphql|auth|oauth|user|users|login)\b[^"\s]*)""#;
const HTTP_RE: &str = r"@(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS|HTTP|Headers?|Query|QueryMap|Path|Body|Field|FieldMap|FormUrlEncoded|Multipart|Part|Url)\b|\bRequest\.Builder|\.url\(|\.newCall\(|HttpURLConnection|openConnection\(|\.addHeader\(|\.header\(|Retrofit\.Builder|\.baseUrl\(|setRequestMethod\(|HttpUrl\.(parse|get)\(|WebSocket";
const SECRET_RE: &str = r#"AIza[0-9A-Za-z_\-]{35}|AKIA[0-9A-Z]{16}|(sk|pk|rk)_(live|test)_[0-9A-Za-z]{16,}|gh[pousr]_[0-9A-Za-z]{36}|xox[baprs]-[0-9A-Za-z\-]{10,}|eyJ[0-9A-Za-z_\-]{10,}\.[0-9A-Za-z_\-]{10,}|(?i)"(authorization|bearer ?|x-api-key|api[_-]?key|x-auth-token|client[_-]?secret|access[_-]?token)"|(?i)\b\w*(api_?key|secret|token|password|passwd|client_?id|private_?key)\w*\s*=\s*"[^"]{6,}"|(?i)name="[^"]*(api_?key|secret|token|client_?id|app_?id|private_?key)[^"]*">[^<\s]{6,}<"#;
/// prefilter for HTTP: types and member names a matching line would mention
const HTTP_TYPES_RE: &str = r"retrofit2/|okhttp3/(Request|HttpUrl|WebSocket|Call|OkHttpClient)|Https?URLConnection|WebSocket";
const HTTP_MEMBERS: [&str; 7] = [
    "openConnection",
    "addHeader",
    "header",
    "newCall",
    "baseUrl",
    "setRequestMethod",
    "url",
];
const SECRET_NAME_RE: &str = r"(?i)api_?key|secret|token|password|passwd|client_?id|private_?key";

/// What one search looks for: an optional user query plus the scope's own pattern.
pub struct Matcher {
    pub scope: Scope,
    /// skip classes from known libraries
    pub app_only: bool,
    query: Option<regex::Regex>,
    builtin: Option<regex::Regex>,
}

impl Matcher {
    pub fn new(q: &str, scope: Scope, use_regex: bool, case: bool) -> Result<Matcher, String> {
        let query = if q.is_empty() {
            None
        } else {
            let pat = if use_regex {
                q.to_string()
            } else {
                regex::escape(q)
            };
            Some(
                regex::RegexBuilder::new(&pat)
                    .case_insensitive(!case)
                    .build()
                    .map_err(|e| e.to_string())?,
            )
        };
        let builtin = match scope {
            Scope::Urls => Some(URL_RE),
            Scope::Http => Some(HTTP_RE),
            Scope::Secrets => Some(SECRET_RE),
            _ => None,
        }
        .map(|p| regex::Regex::new(p).unwrap());
        Ok(Matcher {
            scope,
            app_only: false,
            query,
            builtin,
        })
    }

    fn query_hits(&self, s: &str) -> bool {
        self.query.as_ref().is_none_or(|q| q.is_match(s))
    }

    pub fn line(&self, l: &str) -> bool {
        if self.builtin.is_some() && l.trim_start().starts_with("import ") {
            return false;
        }
        match (&self.builtin, self.scope) {
            // XML namespaces and spec URLs are identifiers, not endpoints
            (Some(b), Scope::Urls) => {
                const NOISE: [&str; 6] = [
                    "schemas.android.com",
                    "www.w3.org",
                    "ns.adobe.com",
                    "www.apache.org/licenses",
                    "xmlpull.org",
                    "schemas.xmlsoap.org",
                ];
                if !b
                    .captures_iter(l)
                    .any(|c| !NOISE.iter().any(|n| c[1].contains(n)))
                {
                    return false;
                }
            }
            (Some(b), _) if !b.is_match(l) => return false,
            _ => {}
        }
        match (&self.query, self.scope) {
            (None, _) => true,
            // only inside a string literal
            (Some(q), Scope::String) => l.split('"').skip(1).step_by(2).any(|lit| q.is_match(lit)),
            (Some(q), _) => q.is_match(l),
        }
    }
}

/// (target: class or resource path, line, text)
pub type Hit = (String, usize, String);

/// Shared with the caller: how far a search got, and a flag to stop it.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub cancel: AtomicBool,
}

/// Classes that can contain a match, from the index; None = every class must be read.
fn candidates(st: &State, m: &Matcher) -> Option<Vec<usize>> {
    let idx = st.index.get()?;
    let wants = |kind: usize| match m.scope {
        Scope::String | Scope::Urls => kind == index::STRING,
        Scope::Usages | Scope::Secrets => true,
        Scope::Http => kind != index::STRING,
        _ => false,
    };
    if !(0..4).any(wants) {
        return None;
    }
    let http_types = regex::Regex::new(HTTP_TYPES_RE).unwrap();
    let secret_names = regex::Regex::new(SECRET_NAME_RE).unwrap();
    let mut hit = vec![false; st.classes.len()];
    let tables: Vec<(usize, usize)> = st
        .dexes
        .iter()
        .enumerate()
        .flat_map(|(d, _)| (0..4).filter(|&k| wants(k)).map(move |k| (d, k)))
        .collect();
    // one table per task: strings of a big DEX are the slow part
    let found = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads().min(tables.len()).max(1) {
            s.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let t = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(d, kind)) = tables.get(t) else {
                        break;
                    };
                    let dx = &st.dexes[d];
                    for id in 0..dx.counts()[kind] {
                        let ok = match kind {
                            index::STRING => dx.string(id as u32).is_ok_and(|s| match m.scope {
                                Scope::String | Scope::Usages => m.query_hits(&s),
                                _ => m.line(&format!("\"{s}\"")),
                            }),
                            index::TYPE => dx.type_name(id as u32).is_ok_and(|t| match m.scope {
                                Scope::Http => http_types.is_match(&t),
                                Scope::Usages => {
                                    m.query_hits(&dex::java_name(&t))
                                        || crate::deobf::class(&t).is_some_and(|n| m.query_hits(n))
                                }
                                _ => false,
                            }),
                            _ => {
                                let (owner, name, ty) = if kind == index::FIELD {
                                    dx.field_parts(id as u32)
                                        .map(|(c, t, n)| (c, n, t))
                                        .unwrap_or_default()
                                } else {
                                    dx.method_parts(id as u32)
                                        .map(|(c, n, _)| (c, n, String::new()))
                                        .unwrap_or_default()
                                };
                                let shown = if kind == index::FIELD {
                                    crate::deobf::field(&owner, &name, &ty)
                                } else {
                                    crate::deobf::method(&owner, &name)
                                };
                                match m.scope {
                                    Scope::Http => {
                                        kind == index::METHOD
                                            && HTTP_MEMBERS.contains(&name.as_str())
                                    }
                                    Scope::Secrets => secret_names.is_match(&name),
                                    Scope::Usages => {
                                        m.query_hits(&name)
                                            || shown.is_some_and(|n| m.query_hits(&n))
                                    }
                                    _ => false,
                                }
                            }
                        };
                        if ok {
                            mine.extend_from_slice(idx.users(d, kind, id));
                        }
                    }
                }
                found.lock().unwrap().extend(mine);
            });
        }
    });
    for k in found.into_inner().unwrap() {
        if let Some(h) = hit.get_mut(k as usize) {
            *h = true;
        }
    }
    Some(
        hit.iter()
            .enumerate()
            .filter(|x| *x.1)
            .map(|x| x.0)
            .collect(),
    )
}

pub fn search(st: &State, m: &Matcher, prog: &Progress) -> Vec<Hit> {
    let mut hits = Vec::new();
    if m.scope == Scope::Class {
        let keep = |k: usize| !m.app_only || st.origin[k] != Origin::Library;
        hits.extend(
            st.classes
                .iter()
                .enumerate()
                .filter(|(k, c)| keep(*k) && m.line(&c.0))
                .take(LIMIT)
                .map(|(_, c)| (c.0.clone(), 0, String::new())),
        );
        return hits;
    }
    if matches!(
        m.scope,
        Scope::Res | Scope::Urls | Scope::Secrets | Scope::Usages
    ) {
        for (p, t) in &st.texts {
            // translations repeat the default strings; API scopes only need res/values
            if m.scope != Scope::Res && p.starts_with("res/values-") {
                continue;
            }
            for (ln, l) in t.lines().enumerate() {
                if m.line(l) {
                    hits.push((p.clone(), ln + 1, l.trim().to_string()));
                }
            }
        }
        if m.scope == Scope::Res {
            hits.extend(
                st.files
                    .keys()
                    .filter(|p| m.line(p))
                    .map(|p| (p.clone(), 0, String::new())),
            );
        }
    }
    // endpoints and keys are often kept in native code; one ABI is enough, the others repeat it
    if matches!(m.scope, Scope::Urls | Scope::Secrets) {
        let libs = st.native_libs();
        let abi = [
            "lib/arm64-v8a/",
            "lib/armeabi-v7a/",
            "lib/x86_64/",
            "lib/x86/",
        ]
        .into_iter()
        .find(|a| libs.iter().any(|l| l.0.starts_with(a)));
        for (lib, _) in libs
            .iter()
            .filter(|l| abi.is_some_and(|a| l.0.starts_with(a)))
        {
            if prog.cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(data) = st.read_entry(lib) else {
                continue;
            };
            for s in elf::strings(&data, 6) {
                if m.line(&format!("\"{s}\"")) {
                    // line 0: the report lists strings in its own order, so there is no line to jump to
                    hits.push((lib.clone(), 0, s));
                }
            }
        }
    }
    if m.scope.decompiles() {
        let mut list = candidates(st, m).unwrap_or_else(|| (0..st.classes.len()).collect());
        if m.app_only {
            list.retain(|&k| st.origin[k] != Origin::Library);
        }
        hits.extend(scan(st, m, &list, prog));
    }
    hits.truncate(LIMIT);
    hits
}

/// Read `list` (class ids) on all cores and keep matching lines. A scan over every class with
/// no cache yet fills the cache on the way.
fn scan(st: &State, m: &Matcher, list: &[usize], prog: &Progress) -> Vec<Hit> {
    prog.total.store(list.len(), Ordering::Relaxed);
    let reader = st.cache.read().unwrap().clone();
    let writer =
        (reader.is_none() && list.len() == st.classes.len() && CACHE_WRITE.load(Ordering::Relaxed))
            .then(|| cache::Writer::create(&st.cache_dir, st.classes.len()))
            .flatten();
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    let full = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            let _ = std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn_scoped(s, || {
                    let mut f = reader.as_ref().and_then(|r| r.file());
                    let mut mine = Vec::new();
                    loop {
                        if prog.cancel.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) {
                            break;
                        }
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&k) = list.get(n) else { break };
                        let text = match (&reader, f.as_mut()) {
                            (Some(r), Some(f)) => st.java_of(k, Some((r, f))),
                            _ => st.java_of(k, None),
                        };
                        if let Some(text) = text {
                            if let Some(w) = &writer {
                                w.put(k, &text);
                            }
                            for (ln, l) in text.lines().enumerate() {
                                if m.line(l) {
                                    mine.push((
                                        st.classes[k].0.clone(),
                                        ln + 1,
                                        l.trim().to_string(),
                                    ));
                                }
                            }
                            // keep scanning to finish the cache, but stop collecting
                            if mine.len() >= LIMIT && writer.is_none() {
                                full.store(true, Ordering::Relaxed);
                            }
                        }
                        prog.done.fetch_add(1, Ordering::Relaxed);
                    }
                    found.lock().unwrap().extend(mine);
                });
        }
    });
    if let Some(w) = writer
        && !prog.cancel.load(Ordering::Relaxed)
        && next.load(Ordering::Relaxed) >= list.len()
        && let Some(r) = w.finish()
    {
        *st.cache.write().unwrap() = Some(Arc::new(r));
    }
    let mut v = found.into_inner().unwrap();
    v.sort();
    v
}

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Csv,
    Json,
}

/// Lines `[from, to)` around a hit: `ctx` lines either side, or the whole enclosing method.
pub fn excerpt(lines: &[&str], at: usize, ctx: usize, method: bool) -> (usize, usize) {
    let indent = |l: &str| l.len() - l.trim_start().len();
    if method {
        const NOT_METHOD: [&str; 9] = [
            "if ",
            "for ",
            "while ",
            "switch ",
            "try ",
            "catch ",
            "} catch",
            "else",
            "synchronized ",
        ];
        let hit_ind = indent(lines.get(at).copied().unwrap_or(""));
        let head = (0..=at.min(lines.len().saturating_sub(1)))
            .rev()
            .find(|&k| {
                let l = lines[k];
                let t = l.trim();
                indent(l) < hit_ind.max(1)
                    && t.ends_with('{')
                    && t.contains('(')
                    && !NOT_METHOD.iter().any(|n| t.starts_with(n))
            });
        if let Some(h) = head {
            let ind = indent(lines[h]);
            let end = (h + 1..lines.len())
                .find(|&k| indent(lines[k]) == ind && lines[k].trim() == "}")
                .map_or(lines.len(), |e| e + 1);
            return (h, end);
        }
    }
    (at.saturating_sub(ctx), (at + ctx + 1).min(lines.len()))
}

pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o += "\\\"",
            '\\' => o += "\\\\",
            '\n' => o += "\\n",
            '\r' => o += "\\r",
            '\t' => o += "\\t",
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Write hits with their surrounding code. `text_of` gives a target's full text.
pub fn export(
    hits: &[Hit],
    ctx: usize,
    method: bool,
    fmt: Format,
    mut text_of: impl FnMut(&str) -> Option<String>,
) -> String {
    let mut out = String::new();
    match fmt {
        Format::Csv => out += "target,line,match,excerpt\n",
        Format::Json => out += "[\n",
        Format::Text => {}
    }
    let mut cur: Option<(String, Option<String>)> = None;
    for (n, (t, ln, text)) in hits.iter().enumerate() {
        if cur.as_ref().is_none_or(|c| &c.0 != t) {
            cur = Some((t.clone(), if ctx > 0 || method { text_of(t) } else { None }));
        }
        let body = cur.as_ref().and_then(|c| c.1.as_deref());
        let snippet = match (body, *ln) {
            (Some(b), l) if l > 0 => {
                let lines: Vec<&str> = b.lines().collect();
                let (a, z) = excerpt(&lines, l - 1, ctx, method);
                lines[a..z].join("\n")
            }
            _ => text.clone(),
        };
        match fmt {
            Format::Text => {
                let _ = writeln!(out, "== {t}:{ln}\n{snippet}\n");
            }
            Format::Csv => {
                let _ = writeln!(
                    out,
                    "{},{ln},{},{}",
                    csv_field(t),
                    csv_field(text),
                    csv_field(&snippet)
                );
            }
            Format::Json => {
                let sep = if n + 1 < hits.len() { "," } else { "" };
                let _ = writeln!(
                    out,
                    "  {{\"target\":{},\"line\":{ln},\"match\":{},\"excerpt\":{}}}{sep}",
                    json_str(t),
                    json_str(text),
                    json_str(&snippet)
                );
            }
        }
    }
    if fmt == Format::Json {
        out += "]\n";
    }
    out
}

/// `name="value"` of the first `name=` in `s`.
pub fn attr(s: &str, name: &str) -> Option<String> {
    let k = s.find(&format!("{name}=\""))? + name.len() + 2;
    Some(s[k..].split('"').next()?.to_string())
}

/// Security-relevant summary of the manifest and APK layout, as readable text.
pub fn overview(st: &State) -> String {
    let m = st
        .texts
        .get("AndroidManifest.xml")
        .map(String::as_str)
        .unwrap_or("");
    // elements as (tag, attributes text, depth-ordered); the decoder writes one element per `<`
    let elems: Vec<(&str, &str)> = m
        .split('<')
        .skip(1)
        .filter_map(|e| {
            let e = e.split('>').next()?;
            let tag = e.split_whitespace().next()?;
            Some((tag, e))
        })
        .collect();
    let a = |e: &str, n: &str| attr(e, &format!("android:{n}"));
    let mut o = String::new();
    let _ = writeln!(o, "// Overview of {}\n", st.path);
    let _ = writeln!(o, "package           {}", st.package);
    if let Some((_, e)) = elems.iter().find(|x| x.0 == "manifest") {
        let _ = writeln!(
            o,
            "version           {} ({})",
            a(e, "versionName").unwrap_or_default(),
            a(e, "versionCode").unwrap_or_default()
        );
    }
    if let Some((_, e)) = elems.iter().find(|x| x.0 == "uses-sdk") {
        let _ = writeln!(
            o,
            "min / target SDK  {} / {}",
            a(e, "minSdkVersion").unwrap_or("?".into()),
            a(e, "targetSdkVersion").unwrap_or("?".into())
        );
    }
    let _ = writeln!(
        o,
        "classes           {} top-level, {} methods, {} dex",
        st.classes.len(),
        st.methods,
        st.dexes.len()
    );
    if let Some((_, e)) = elems.iter().find(|x| x.0 == "application") {
        let _ = writeln!(o, "\n// application flags");
        for f in [
            "debuggable",
            "allowBackup",
            "usesCleartextTraffic",
            "networkSecurityConfig",
            "requestLegacyExternalStorage",
            "name",
        ] {
            if let Some(v) = a(e, f) {
                let warn = matches!(
                    (f, v.as_str()),
                    ("debuggable", "true")
                        | ("usesCleartextTraffic", "true")
                        | ("allowBackup", "true")
                );
                let _ = writeln!(o, "{f:<29}{v}{}", if warn { "    <- review" } else { "" });
            }
        }
    }
    const DANGEROUS: [&str; 16] = [
        "READ_CONTACTS",
        "WRITE_CONTACTS",
        "READ_SMS",
        "SEND_SMS",
        "RECEIVE_SMS",
        "READ_CALL_LOG",
        "CALL_PHONE",
        "RECORD_AUDIO",
        "CAMERA",
        "ACCESS_FINE_LOCATION",
        "ACCESS_COARSE_LOCATION",
        "ACCESS_BACKGROUND_LOCATION",
        "READ_PHONE_STATE",
        "READ_EXTERNAL_STORAGE",
        "BODY_SENSORS",
        "REQUEST_INSTALL_PACKAGES",
    ];
    let perms: Vec<String> = elems
        .iter()
        .filter(|x| x.0.starts_with("uses-permission"))
        .filter_map(|x| a(x.1, "name"))
        .collect();
    let _ = writeln!(o, "\n// permissions ({})", perms.len());
    for p in &perms {
        let flag = DANGEROUS.iter().any(|d| p.ends_with(d));
        let _ = writeln!(o, "{p}{}", if flag { "    <- dangerous" } else { "" });
    }
    // components: exported explicitly, or implicitly through an intent-filter
    let _ = writeln!(o, "\n// exported components (reachable by other apps)");
    let mut deep = Vec::new();
    // (tag, name, exported attribute, has an intent-filter)
    let mut cur: Option<(String, String, Option<String>, bool)> = None;
    let flush = |c: Option<(String, String, Option<String>, bool)>, o: &mut String| {
        if let Some((tag, name, exp, filt)) = c {
            // before Android 12 a component with an intent-filter and no `exported` is exported
            match (exp.as_deref(), filt) {
                (Some("true"), _) => {
                    let _ = writeln!(o, "{tag:<10}{name}");
                }
                (None, true) => {
                    let _ = writeln!(o, "{tag:<10}{name}    (exported via intent-filter)");
                }
                _ => {}
            }
        }
    };
    for (tag, e) in &elems {
        match *tag {
            "activity" | "activity-alias" | "service" | "receiver" | "provider" => {
                flush(cur.take(), &mut o);
                cur = Some((
                    tag.to_string(),
                    a(e, "name").unwrap_or_default(),
                    a(e, "exported"),
                    false,
                ));
            }
            "intent-filter" => {
                if let Some(c) = cur.as_mut() {
                    c.3 = true;
                }
            }
            "data" => {
                let (s, h, p) = (
                    a(e, "scheme"),
                    a(e, "host"),
                    a(e, "path").or(a(e, "pathPrefix")).or(a(e, "pathPattern")),
                );
                if s.is_some() || h.is_some() {
                    deep.push(format!(
                        "{}://{}{}",
                        s.unwrap_or("*".into()),
                        h.unwrap_or("*".into()),
                        p.unwrap_or_default()
                    ));
                }
            }
            _ => {}
        }
    }
    flush(cur.take(), &mut o);
    deep.sort();
    deep.dedup();
    let _ = writeln!(o, "\n// deep links ({})", deep.len());
    for d in &deep {
        let _ = writeln!(o, "{d}");
    }
    let _ = writeln!(
        o,
        "
// signature"
    );
    o += &crate::sign::summary(&st.file, &st.entries, |p| st.read_entry(p));
    let libs: Vec<&String> = st
        .entries
        .iter()
        .filter(|e| e.starts_with("lib/") && e.ends_with(".so"))
        .collect();
    let _ = writeln!(o, "\n// native libraries ({})", libs.len());
    for l in &libs {
        let _ = writeln!(o, "{l}");
    }
    let assets = st
        .entries
        .iter()
        .filter(|e| e.starts_with("assets/"))
        .count();
    let _ = writeln!(o, "\n// assets: {assets} files");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_scopes_match() {
        let urls = Matcher::new("", Scope::Urls, false, false).unwrap();
        assert!(urls.line(r#"String u = "https://api.example.com/v1/login";"#));
        assert!(urls.line(r#"@GET("/api/users/{id}")"#));
        assert!(!urls.line(r#"String s = "hello world";"#));
        assert!(!urls.line(r#"xmlns:android="http://schemas.android.com/apk/res/android""#));
        let http = Matcher::new("", Scope::Http, false, false).unwrap();
        assert!(
            http.line(r#"@POST("v2/token")"#)
                && http.line("Request request = new Request.Builder().url(str).build();")
        );
        assert!(!http.line("int getHeight() {"));
        assert!(
            !http.line("IntentSenderRequest.Builder builder = new IntentSenderRequest.Builder(x);")
        );
        assert!(!http.line("import okhttp3.Request.Builder;"));
        let sec = Matcher::new("", Scope::Secrets, false, false).unwrap();
        assert!(sec.line(
            r#"<string name="google_api_key">AIzaSyA1234567890abcdefghijklmnopqrstuv</string>"#
        ));
        assert!(sec.line(r#"builder.addHeader("Authorization", "Bearer " + token);"#));
        assert!(!sec.line("return x + 1;"));
        assert!(
            !sec.line(
                r#"<string name="password_toggle_content_description">Show password</string>"#
            )
        );
        assert!(sec.line(r#"private static final String API_KEY = "k3y-1234567890";"#));
        assert!(sec.line(r#"<string name="facebook_client_token">abcdef123456</string>"#));
        // query narrows a scope; String scope looks only inside literals
        let narrowed = Matcher::new("example", Scope::Urls, false, false).unwrap();
        assert!(
            narrowed.line(r#""https://example.com""#) && !narrowed.line(r#""https://other.com""#)
        );
        let lit = Matcher::new("abc", Scope::String, false, false).unwrap();
        assert!(lit.line(r#"f("xabcx");"#) && !lit.line("abc();"));
        assert!(Matcher::new("(", Scope::Code, true, false).is_err());
    }

    #[test]
    fn excerpts_and_formats() {
        let src = "class A {\n    void f(int x) {\n        if (x > 0) {\n            call(\"/api/auth\");\n        }\n    }\n\n    void g() {\n    }\n}";
        let lines: Vec<&str> = src.lines().collect();
        // whole method: from the header to its closing brace, skipping the `if`
        assert_eq!(excerpt(&lines, 3, 0, true), (1, 6));
        assert_eq!(excerpt(&lines, 3, 1, false), (2, 5));
        assert_eq!(excerpt(&lines, 0, 5, false), (0, 6));
        let hits = vec![("A".to_string(), 4, "call(\"/api/auth\");".to_string())];
        let json = export(&hits, 0, true, Format::Json, |_| Some(src.to_string()));
        assert!(
            json.contains("\"excerpt\":\"    void f(int x) {\\n") && json.trim_end().ends_with(']'),
            "{json}"
        );
        let csv = export(&hits, 0, false, Format::Csv, |_| None);
        assert_eq!(
            csv.lines().nth(1),
            Some("\"A\",4,\"call(\"\"/api/auth\"\");\",\"call(\"\"/api/auth\"\");\"")
        );
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin("oncode.apps.zestle.MainActivity", "oncode.apps.zestle"),
            Origin::App
        );
        // a sibling package with the same prefix is not the app
        // code beside the launcher package is the app's too, a different organization is not
        assert_eq!(
            origin("com.unciv.logic.GameInfo", "com.unciv.app"),
            Origin::App
        );
        assert_eq!(origin("com.uncivx.A", "com.unciv.app"), Origin::Other);
        assert_eq!(
            origin("androidx.core.view.ViewCompat", "oncode.apps.zestle"),
            Origin::Library
        );
        assert_eq!(origin("pkg_a.ClassB", "oncode.apps.zestle"), Origin::Other);
        assert_eq!(origin("com.google.gson.Gson", ""), Origin::Library);
    }

    #[test]
    fn manifest_attrs() {
        assert_eq!(
            attr("<x android:name=\"a.B\" package=\"p.q\">", "package").as_deref(),
            Some("p.q")
        );
        assert_eq!(attr("<x>", "package"), None);
    }
}

/// Smali of class `c` and its nested classes, outer first.
pub fn smali(dx: &Dex, c: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![c];
    while let Some(x) = stack.pop() {
        let _ = writeln!(
            out,
            ".class {}
",
            dx.class_descriptor(x).unwrap_or_default()
        );
        for m in dx.methods(x).unwrap_or_default() {
            let _ = writeln!(
                out,
                ".method {:#x} {}",
                m.access,
                dx.method_ref(m.method_idx).unwrap_or_default()
            );
            if let Some(code) = &m.code {
                let _ = writeln!(out, "    .registers {}", code.registers);
                for i in bytecode::decode(code.insns).unwrap_or_default() {
                    let _ = writeln!(
                        out,
                        "    {:04x}: {}",
                        i.off,
                        crate::render(dx, &i).unwrap_or_else(|e| e.to_string())
                    );
                }
            }
            out += ".end method

";
        }
        stack.extend(dx.nesting().kids[x].iter().rev());
    }
    out
}
