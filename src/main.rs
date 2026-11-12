mod cache;
mod cfg;
mod deobf;
mod dex;
mod elf;
mod export;
#[cfg(feature = "gui")]
mod gui;
mod il2cpp;
mod imports;
mod index;
mod ir;
mod java;
mod live;
mod mcp;
mod opt;
mod quick;
mod re;
mod session;

// the editing core is its own crate, usable as a library
use apkscope_core::dex::render;
use apkscope_core::{apksign, arsc, axml, bytecode, rebuild, res, sign, smali};

use std::fmt::Write as _;
use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    process::ExitCode,
};

// returns freed memory to the OS, so big APKs don't keep loading-time peaks resident
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn usage() -> ExitCode {
    eprintln!("apkscope {} - Android APK decompiler, API recon and editor
Fast, stateless queries (parallel, no index, no cache) - built for scripts and AI agents:
  apkscope classes  <apk> [--prefix com.example] [--app-only] [--desc]
  apkscope methods  <apk> [--class com.example.Main] [--filter name]
  apkscope strings  <apk> [--filter text]
  apkscope refs     <apk> string|type|method|field|extends|impl <query>
                          [--class C] [--fuzzy-class] [--regex] [--ignore-case] [--java]
  apkscope decompile <apk> <Class>... [--method name] [--smali] [--full]
  apkscope manifest <apk>
  apkscope diff     <old.apk> <new.apk> [--strings]   added / removed classes, methods, permissions
Indexed + cached (first run builds it, later runs reuse the disk cache):
  apkscope decompile <apk>                 every class, to stdout
  apkscope search <apk> <scope> [query] [--regex] [--case] [--app-only] [--context N] [--method]
                  [--format txt|csv|json]   scopes: code string usages class res urls http secrets
  apkscope info <apk>                      package, SDKs, permissions, exported components
  apkscope resource <apk> <res/path.xml>
  apkscope export <apk> <out-dir>          Gradle project
Reverse engineering (Class = a.b.C, member = a.b.C#field or a.b.C#method()):
  apkscope callers   <apk> <Class> [method]      who calls it
  apkscope callgraph <apk> <Class> [method] [--depth N]   callers + callee tree
  apkscope access    <apk> <Class> <field>       who reads / writes a field
  apkscope frida     <apk> <Class> [method]      Frida hook script (--live: event body for live sessions)
  apkscope dump      <apk|package> <out-dir>     run the app under Frida, save the DEX it unpacked and the
                                                 scripts it decrypted (--wait S, --scripts-for S, --attach, --device ID)
  apkscope il2cpp    <apk|xapk>... [--uses TEXT] [--method NAME]   Unity games: C# methods + addresses,
                                                 which method uses a string, strings/calls in a method
  apkscope flutter   <apk> [--abi NAME] [--api] [--strings]   Flutter apps: Dart snapshot version, native libs,
                                                 recovered Dart library tree; --api lists hosts/routes/model
                                                 fields; --strings dumps libapp.so strings
  apkscope scan      <apk>                       security quick-scan
  apkscope decrypt   <apk>                       run string decryptors on their constant arguments
  apkscope rename    <apk> <Class|member> <new>  saved in <apk>.apkscope.json (empty name removes it)
  apkscope comment   <apk> <Class|member> <text>
  apkscope map <apk> export <out.jobf|out.mapping>  |  map <apk> import <file>   JADX / Enigma names
Editing: apkscope smali <apk> <Class>      editable smali of one class (edit, then build --smali)
         apkscope build <in.apk> <out.apk> [--put name=file]... [--delete name]... [--manifest x.xml]
                  [--string name[@config]=value]... [--smali edited.smali]...
                  [--key k.pk8 --cert c.crt | --no-sign]   a .jar is never signed
         apkscope verify <apk>
Agents:  apkscope mcp                      MCP server over stdio (see README)
Other:   apkscope disasm <apk> [Class]     apkscope gui [apk]
Global options: --json  --limit N  --offset N  --threads N  -o/--out file  --no-deobf  --no-cache", env!("CARGO_PKG_VERSION"));
    ExitCode::from(2)
}

/// Global output options, set once from the command line.
struct Out {
    file: Option<String>,
    json: bool,
    limit: usize,
    offset: usize,
}

static OUT: std::sync::OnceLock<Out> = std::sync::OnceLock::new();

fn out_opts() -> &'static Out {
    OUT.get_or_init(|| Out {
        file: None,
        json: false,
        limit: usize::MAX,
        offset: 0,
    })
}

/// Write the result to `-o` or stdout.
fn emit(text: &str) -> Result<(), String> {
    emit_with(|w| w.write_all(text.as_bytes()))
}

/// Stream output into `-o` or stdout through one buffer.
fn emit_with(f: impl FnOnce(&mut dyn Write) -> std::io::Result<()>) -> Result<(), String> {
    let res = match &out_opts().file {
        Some(path) => File::create(path).and_then(|file| {
            let mut w = BufWriter::with_capacity(1 << 20, file);
            f(&mut w)?;
            w.flush()
        }),
        None => {
            let mut w = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
            f(&mut w).and_then(|_| w.flush())
        }
    };
    match res {
        // a closed pipe (`| head`) is not an error
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => r.map_err(|e| format!("{}: {e}", out_opts().file.as_deref().unwrap_or("stdout"))),
    }
}

/// Write each piece as `run` produces it (in order), instead of collecting everything first:
/// the reader gets the first lines while later DEX files are still being worked on.
fn emit_stream<T>(
    run: impl FnOnce(&mut dyn FnMut(Result<T, String>)) -> Result<(), String>,
    put: impl Fn(&mut dyn Write, T) -> std::io::Result<()>,
) -> Result<(), String> {
    let (mut io_err, mut piece_err, mut res) = (None, None, Ok(()));
    emit_with(|w| {
        res = run(&mut |t| match t {
            Ok(t) if io_err.is_none() => io_err = put(&mut *w, t).err(),
            Ok(_) => {}
            Err(e) => {
                piece_err.get_or_insert(e);
            }
        });
        io_err.take().map_or(Ok(()), Err)
    })?;
    res.and(piece_err.map_or(Ok(()), Err))
}

/// One item per line, or a JSON array with `--json`; `--offset`/`--limit` page through it.
fn emit_list(items: &[String]) -> Result<(), String> {
    let o = out_opts();
    let page = || items.iter().skip(o.offset).take(o.limit);
    let shown = page().count();
    if shown < items.len() {
        eprintln!("{shown} of {} shown (--offset/--limit)", items.len());
    }
    emit_with(|w| {
        if o.json {
            w.write_all(b"[")?;
            for (k, s) in page().enumerate() {
                if k > 0 {
                    w.write_all(b",")?;
                }
                w.write_all(session::json_str(s).as_bytes())?;
            }
            w.write_all(
                b"]
",
            )
        } else {
            for s in page() {
                w.write_all(s.as_bytes())?;
                w.write_all(
                    b"
",
                )?;
            }
            Ok(())
        }
    })
}

/// Remove `--name` from `args`; true if it was there.
fn flag(args: &mut Vec<String>, name: &str) -> bool {
    let n = args.len();
    args.retain(|a| a != name);
    args.len() != n
}

/// Remove `--name value` from `args` and return the value.
fn opt(args: &mut Vec<String>, names: &[&str]) -> Result<Option<String>, String> {
    let Some(k) = args.iter().position(|a| names.contains(&a.as_str())) else {
        return Ok(None);
    };
    if k + 1 >= args.len() {
        return Err(format!("{} needs a value", names[0]));
    }
    args.remove(k);
    Ok(Some(args.remove(k)))
}

fn num(v: Option<String>, name: &str) -> Result<Option<usize>, String> {
    v.map(|v| v.parse().map_err(|_| format!("{name} needs a number")))
        .transpose()
}

/// `refs` words after the APK: kind, query and flags. Returns the query and `--java`.
fn refs_query(mut args: Vec<String>) -> Result<(quick::Query, bool), String> {
    let class = opt(&mut args, &["--class"])?;
    let (fuzzy, regex, ic, java) = (
        flag(&mut args, "--fuzzy-class"),
        flag(&mut args, "--regex"),
        flag(&mut args, "--ignore-case"),
        flag(&mut args, "--java"),
    );
    let [kind, query @ ..] = args.as_slice() else {
        return Err("usage: apkscope refs <apk> <kind> <query>".into());
    };
    let kind = quick::Kind::parse(kind).ok_or_else(|| {
        format!("unknown kind {kind}: string, type, method, field, extends, impl")
    })?;
    let q = quick::Query {
        kind,
        text: query.join(" "),
        class,
        fuzzy_class: fuzzy,
        regex,
        ignore_case: ic,
    };
    if q.text.is_empty() {
        return Err("refs needs a query".into());
    }
    Ok((q, java))
}

/// Java context for `refs_text`: decompiles the given classes,
/// (class, the lines of its text that mention the query).
type JavaOf<'a> = &'a dyn Fn(&[String]) -> Vec<(String, Option<String>)>;

/// Hits as text or JSON; with `java_of`, each referencing class's decompiled lines that mention
/// the query (one decompile per class).
fn refs_text(
    q: &quick::Query,
    hits: &[quick::Ref_],
    json: bool,
    java_of: Option<JavaOf<'_>>,
) -> Result<String, String> {
    let mut code: std::collections::HashMap<String, Vec<String>> = Default::default();
    if let Some(java_of) = java_of {
        let mut owners: Vec<String> = hits
            .iter()
            .map(|h| h.1.split("->").next().unwrap_or(&h.1).to_string())
            .collect();
        owners.sort();
        owners.dedup();
        let needle = match q.kind {
            quick::Kind::Type => q
                .text
                .trim_end_matches(';')
                .rsplit(['.', '/', '$'])
                .next()
                .unwrap_or(&q.text)
                .to_string(),
            _ => q.text.clone(),
        };
        let pat = if q.regex {
            needle
        } else {
            ::regex::escape(&needle)
        };
        let re = ::regex::RegexBuilder::new(&pat)
            .case_insensitive(q.ignore_case)
            .build()
            .map_err(|e| e.to_string())?;
        for (name, text) in java_of(&owners) {
            let lines = text
                .map(|j| {
                    j.lines()
                        .enumerate()
                        .filter(|(_, l)| re.is_match(l) && !l.starts_with("import "))
                        .map(|(n, l)| format!("{:>5}: {}", n + 1, l.trim()))
                        .collect()
                })
                .unwrap_or_default();
            code.insert(name, lines);
        }
    }
    let mut text = String::new();
    if json {
        let rows: Vec<String> = hits
            .iter()
            .map(|(d, m, w)| {
                let owner = m.split("->").next().unwrap_or(m);
                let java = code
                    .get(owner)
                    .map(|l| {
                        format!(
                            ",\"java\":[{}]",
                            l.iter()
                                .map(|x| session::json_str(x))
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{{\"dex\":{},\"from\":{},\"match\":{}{java}}}",
                    session::json_str(d),
                    session::json_str(m),
                    session::json_str(w)
                )
            })
            .collect();
        text = format!("[\n{}\n]\n", rows.join(",\n"));
    } else {
        let mut shown = std::collections::HashSet::new();
        for (d, m, w) in hits {
            let _ = writeln!(text, "{d} | {m} | matched={}", w.replace('\n', "\\n"));
            let owner = m.split("->").next().unwrap_or(m);
            if let Some(lines) = code.get(owner).filter(|_| shown.insert(owner.to_string())) {
                for l in lines {
                    let _ = writeln!(text, "    {l}");
                }
            }
        }
    }
    Ok(text)
}

fn refs_cmd(mut args: Vec<String>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: apkscope refs <apk> <kind> <query>".into());
    }
    let apk = args.remove(0);
    let (q, java) = refs_query(args)?;
    let t = std::time::Instant::now();
    let o = out_opts();
    let hits = quick::refs(&apk, &q, o.offset.saturating_add(o.limit))?;
    let hits = &hits[o.offset.min(hits.len())..];
    // raw names, so the query text matches the decompiled lines
    let java_of = |owners: &[String]| {
        quick::decompile(&apk, owners, false, false)
            .map(|v| v.into_iter().map(|(n, r)| (n, r.ok())).collect())
            .unwrap_or_default()
    };
    let text = refs_text(&q, hits, o.json, if java { Some(&java_of) } else { None })?;
    emit(&text)?;
    eprintln!(
        "{} refs in {:.0} ms",
        hits.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn decompile_cmd(mut args: Vec<String>, deobf: bool) -> Result<(), String> {
    let method = opt(&mut args, &["--method"])?;
    let (smali, full) = (flag(&mut args, "--smali"), flag(&mut args, "--full"));
    let [apk, names @ ..] = args.as_slice() else {
        return Err("usage: apkscope decompile <apk> [Class]...".into());
    };
    if names.is_empty() {
        return decompile(apk, None, deobf);
    }
    let results: Vec<(String, Result<String, String>)> = if full {
        // whole-APK names: same output as the viewer and export
        let st = load(apk, deobf)?;
        names
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    st.class_text(n, smali)
                        .ok_or_else(|| format!("class {n} not found")),
                )
            })
            .collect()
    } else {
        quick::decompile(apk, names, smali, deobf)?
    };
    let mut text = String::new();
    let mut missing = Vec::new();
    for (name, r) in results {
        match r {
            Ok(t) => match &method {
                Some(m) => match quick::method_of(&t, m) {
                    Some(x) => text += &x,
                    None => missing.push(format!("method {m} not found in {name}")),
                },
                None => {
                    text += &t;
                    text.push('\n');
                }
            },
            Err(e) => missing.push(e),
        }
    }
    emit(&text)?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join("; "))
    }
}

fn il2cpp_cmd(mut a: Vec<String>) -> Result<(), String> {
    let uses = opt(&mut a, &["--uses"])?;
    let method = opt(&mut a, &["--method"])?;
    if a.is_empty() {
        return Err("usage: apkscope il2cpp <apk|xapk>... [--uses TEXT] [--method NAME]".into());
    }
    let g = il2cpp::load(&a)?;
    let name = |addr: u64| match g.method_at(addr) {
        Some((m, 0)) => format!("0x{:x}  {}  {}", m.addr, m.image, m.name),
        Some((m, n)) => format!(
            "0x{:x}  {}  {}  (+{n} share this code)",
            m.addr, m.image, m.name
        ),
        None => format!("0x{addr:x}  ?"),
    };
    let mut out = Vec::new();
    if let Some(text) = uses {
        let mut hits: std::collections::BTreeMap<usize, std::collections::BTreeSet<u64>> =
            Default::default();
        for (pc, k) in g.literal_uses() {
            if g.literals[k].contains(&text) {
                hits.entry(k)
                    .or_default()
                    .insert(g.method_at(pc).map_or(pc, |(m, _)| m.addr));
            }
        }
        for (k, at) in hits {
            out.push(session::json_str(&g.literals[k]));
            out.extend(at.into_iter().map(|a| format!("  {}", name(a))));
        }
    } else if let Some(f) = method {
        for m in g.methods.iter().filter(|m| m.name.contains(&f)) {
            out.push(format!("{}  {}  0x{:x}", m.image, m.name, m.addr));
            let (lits, calls) = g.scan(m.addr, g.end_of(m.addr));
            out.extend(lits.iter().map(|&(pc, k)| {
                format!("  0x{pc:x}  string  {}", session::json_str(&g.literals[k]))
            }));
            // direct calls to known method starts; shared bodies show one of the names
            out.extend(
                calls
                    .iter()
                    .filter(|&&(_, t)| g.method_at(t).is_some_and(|(c, _)| c.addr == t))
                    .map(|&(pc, t)| format!("  0x{pc:x}  call    {}", name(t))),
            );
        }
    } else {
        out = g
            .methods
            .iter()
            .map(|m| format!("0x{:x}  {}  {}", m.addr, m.image, m.name))
            .collect();
    }
    emit_list(&out)
}

fn manifest_cmd(apk: &str) -> Result<(), String> {
    let m = quick::read_entry(apk, "AndroidManifest.xml")?.ok_or("no AndroidManifest.xml")?;
    let names =
        quick::read_entry(apk, "resources.arsc")?.and_then(|d| res::Names::from_arsc(&d).ok());
    emit(&res::decode_xml(&m, names.as_ref())?)
}

/// Full session; `--no-cache` keeps it from reading or writing the code cache.
fn load(apk: &str, deobf: bool) -> Result<session::State, String> {
    let st = session::load(apk, deobf)?;
    if !session::CACHE_WRITE.load(std::sync::atomic::Ordering::Relaxed) {
        st.drop_cache();
    }
    Ok(st)
}

/// Calls `f` with each root-level classes*.dex; stops early when `f` returns Ok(true).
fn for_each_dex(
    path: &str,
    mut f: impl FnMut(&str, &dex::Dex) -> Result<bool, String>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{path}: {e}"))?;
    let mut buf = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        // only root-level classes*.dex are loaded by Android
        if !(name.starts_with("classes") && name.ends_with(".dex") && !name.contains('/')) {
            continue;
        }
        buf.clear();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("{name}: {e}"))?;
        let dex = dex::Dex::parse(&buf).map_err(|e| format!("{name}: {e}"))?;
        if f(&name, &dex).map_err(|e| format!("{name}: {e}"))? {
            break;
        }
    }
    Ok(())
}

fn disasm(path: &str, class: Option<&str>) -> Result<(), String> {
    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut found = false;
    for_each_dex(path, |_, dex| {
        for c in 0..dex.class_count {
            let name = dex::java_name(&dex.class_descriptor(c)?);
            if class.is_some_and(|want| want != name) {
                continue;
            }
            found = true;
            for m in dex.methods(c)? {
                let _ = writeln!(
                    out,
                    "\n.method {:#x} {}",
                    m.access,
                    dex.method_ref(m.method_idx)?
                );
                let Some(code) = m.code else { continue };
                let _ = writeln!(out, "    .registers {} (ins {})", code.registers, code.ins);
                for i in bytecode::decode(code.insns)? {
                    let _ = writeln!(out, "    {:04x}: {}", i.off, render(dex, &i)?);
                }
            }
            if class.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    })?;
    let _ = out.flush();
    match class {
        Some(c) if !found => Err(format!("class {c} not found")),
        _ => Ok(()),
    }
}

fn decompile(path: &str, class: Option<&str>, deobf: bool) -> Result<(), String> {
    let apk = export::open(&session::prepare(path)?)?;
    // 'static so inline string decryption can reach every DEX
    let raw: &'static [Vec<u8>] = Box::leak(
        apk.dex
            .into_iter()
            .map(|(_, b)| b)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let dexes: Vec<dex::Dex<'static>> =
        raw.iter().filter_map(|b| dex::Dex::parse(b).ok()).collect();
    if deobf {
        deobf::init(&dexes);
    }
    deobf::set_notes(re::load_notes(path)?);
    re::set_dexes(Box::leak(
        raw.iter()
            .filter_map(|b| dex::Dex::parse(b).ok())
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    ));
    crate::imports::init(&dexes);
    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut found = false;
    'all: for dex in &dexes {
        let nest = dex.nesting();
        for (c, parent) in nest.parent.iter().enumerate() {
            let desc = dex.class_descriptor(c).unwrap_or_default();
            // accept the readable name or the original one
            let raw = desc
                .trim_start_matches('L')
                .trim_end_matches(';')
                .replace('/', ".");
            match class {
                Some(want) if want != dex::java_name(&desc) && want != raw => continue,
                None if parent.is_some() => continue, // printed inside its outer class
                _ => {}
            }
            found = true;
            match java::class(dex, c) {
                Ok(s) => {
                    let _ = writeln!(out, "{s}");
                }
                Err(e) => {
                    let _ = writeln!(out, "/* class {c} failed: {e} */");
                }
            }
            if class.is_some() {
                break 'all;
            }
        }
    }
    let _ = out.flush();
    match class {
        Some(c) if !found => Err(format!("class {c} not found")),
        _ => Ok(()),
    }
}

/// Structuring recurses per nesting level; deep methods need more than Windows' 1 MB main stack.
fn run_big_stack(f: impl FnOnce() -> Result<(), String> + Send) -> Result<(), String> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn_scoped(s, f)
            .unwrap()
            .join()
            .unwrap()
    })
}

fn export_cmd(path: &str, out: &str, deobf: bool) -> Result<(), String> {
    let start = std::time::Instant::now();
    let mut apk = export::open(&session::prepare(path)?)?;
    let root = std::path::Path::new(out);
    let main_dir = root.join("app/src/main");
    let (n, failed) = export::sources(&apk, &main_dir.join("java"), deobf);
    eprintln!(
        "sources: {n} classes ({failed} failed) in {:.1}s",
        start.elapsed().as_secs_f64()
    );
    let t = std::time::Instant::now();
    let (st, manifest) = export::resources(&mut apk, &main_dir)?;
    export::gradle(root, &manifest.unwrap_or_default())?;
    eprintln!(
        "resources: {} values files, {} res files ({} binary XML decoded), {} assets/libs in {:.1}s",
        st.values,
        st.files,
        st.xml_decoded,
        st.other,
        t.elapsed().as_secs_f64()
    );
    eprintln!(
        "done in {:.1}s -> {}",
        start.elapsed().as_secs_f64(),
        root.display()
    );
    Ok(())
}

/// Started by double-click: drop the console window Windows opened for this console program.
fn hide_console() {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn FreeConsole() -> i32;
        }
        // safety: no arguments; only detaches this process from its console
        unsafe { FreeConsole() };
    }
}

/// Parsed `search` options; `pos` keeps the positional words (apk, scope, query...).
struct SearchArgs {
    pos: Vec<String>,
    regex: bool,
    case: bool,
    method: bool,
    app_only: bool,
    ctx: usize,
    fmt: session::Format,
    out: Option<String>,
}

fn search_args(args: &[String]) -> Result<SearchArgs, String> {
    let mut s = SearchArgs {
        pos: Vec::new(),
        regex: false,
        case: false,
        method: false,
        app_only: false,
        ctx: 0,
        fmt: session::Format::Text,
        out: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--regex" => s.regex = true,
            "--case" => s.case = true,
            "--method" => s.method = true,
            "--app-only" => s.app_only = true,
            "--context" => {
                s.ctx = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--context needs a number")?
            }
            "--format" => {
                s.fmt = match it.next().map(String::as_str) {
                    Some("txt") => session::Format::Text,
                    Some("csv") => session::Format::Csv,
                    Some("json") => session::Format::Json,
                    _ => return Err("--format is txt, csv or json".into()),
                }
            }
            "--out" | "-o" => s.out = Some(it.next().ok_or("--out needs a file")?.clone()),
            _ => s.pos.push(a.clone()),
        }
    }
    Ok(s)
}

/// Run one search on a loaded session; `words` = [scope, query...]. Returns (text, hits, note).
fn search_on(
    st: &session::State,
    a: &SearchArgs,
    words: &[String],
) -> Result<(String, usize, &'static str), String> {
    let [scope, rest @ ..] = words else {
        return Err("search needs a scope".into());
    };
    let scope = session::Scope::parse(scope).ok_or_else(|| {
        format!("unknown scope {scope}: code string usages class res urls http secrets")
    })?;
    let mut m = session::Matcher::new(&rest.join(" "), scope, a.regex, a.case)?;
    m.app_only = a.app_only;
    if scope.decompiles() && st.index.get().is_none() {
        st.build_index();
    }
    let hits = session::search(st, &m, &session::Progress::default());
    Ok((
        session::export(&hits, a.ctx, a.method, a.fmt, |x| st.target_text(x)),
        hits.len(),
        scope.note(),
    ))
}

/// `apkscope search`: same engine as the viewer; results go to stdout or `--out`.
fn search_cmd(args: &[String], deobf: bool) -> Result<(), String> {
    let a = search_args(args)?;
    let [apk, words @ ..] = a.pos.as_slice() else {
        return Err("usage: apkscope search <file.apk> <scope> [query]".into());
    };
    let t = std::time::Instant::now();
    let st = load(apk, deobf)?;
    let loaded = t.elapsed();
    let (text, n, note) = search_on(&st, &a, words)?;
    match &a.out {
        Some(f) => std::fs::write(f, text).map_err(|e| format!("{f}: {e}"))?,
        None => {
            let _ = std::io::stdout().lock().write_all(text.as_bytes());
        }
    }
    eprintln!(
        "{n} hits | load {:.2}s, index + search {:.2}s{}",
        loaded.as_secs_f64(),
        (t.elapsed() - loaded).as_secs_f64(),
        if st.cached() {
            ", code cache present"
        } else {
            ""
        }
    );
    eprintln!("note: {note}");
    Ok(())
}

/// `apkscope build`: apply edits, zipalign, sign v2 + v3, then verify the result. `--no-sign`
/// leaves the APK unsigned; a JAR never is signed, Android does not verify one.
fn build_cmd(args: &[String]) -> Result<(), String> {
    let mut pos = Vec::new();
    let mut edits = rebuild::Edits::new();
    let (mut key, mut cert, mut manifest) = (None, None, None);
    let mut sign = true;
    let mut strings = std::collections::BTreeMap::new();
    let mut smali_srcs: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--put" => {
                let v = it.next().ok_or("--put needs name=file")?;
                let (name, file) = v.split_once('=').ok_or("--put needs name=file")?;
                edits.insert(
                    name.to_string(),
                    Some(std::fs::read(file).map_err(|e| format!("{file}: {e}"))?),
                );
            }
            "--delete" => {
                edits.insert(it.next().ok_or("--delete needs a name")?.clone(), None);
            }
            "--no-sign" => sign = false,
            "--string" => {
                let v = it.next().ok_or("--string needs name=value")?;
                let (k, text) = v.split_once('=').ok_or("--string needs name=value")?;
                let (name, config) = k.split_once('@').unwrap_or((k, ""));
                strings.insert((config.to_string(), name.to_string()), text.to_string());
            }
            "--smali" => {
                let f = it.next().ok_or("--smali needs a .smali file")?;
                smali_srcs.push(std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?);
            }
            "--manifest" => {
                manifest = Some(it.next().ok_or("--manifest needs an XML file")?.clone())
            }
            "--key" => key = Some(it.next().ok_or("--key needs a file")?.clone()),
            "--cert" => cert = Some(it.next().ok_or("--cert needs a file")?.clone()),
            _ => pos.push(a.clone()),
        }
    }
    let [input, output] = pos.as_slice() else {
        return Err("usage: apkscope build <in.apk> <out.apk> [options]".into());
    };
    let jar = input.to_lowercase().ends_with(".jar");
    if (jar || !sign) && (key.is_some() || cert.is_some()) {
        return Err("--key and --cert sign an APK: not a .jar, and not with --no-sign".into());
    }
    let key = match (key, cert) {
        _ if jar || !sign => None,
        (Some(k), Some(c)) => Some(apksign::load(
            std::path::Path::new(&k),
            std::path::Path::new(&c),
        )?),
        (None, None) => {
            let (k, dir) = apksign::default_key()?;
            eprintln!("signing with apkscope's key in {}", dir.display());
            Some(k)
        }
        _ => return Err("--key and --cert go together".into()),
    };
    let t = std::time::Instant::now();
    let src = session::prepare(input)?;
    if let Some(m) = manifest {
        // text manifest -> binary XML, typed like the original
        let text = std::fs::read_to_string(&m).map_err(|e| format!("{m}: {e}"))?;
        let mut a = export::open(&src)?;
        let table = a
            .read("resources.arsc")
            .and_then(|d| res::parse_table(&d).ok());
        let names = table
            .as_ref()
            .map(res::Names::new)
            .ok_or("no resources.arsc to resolve @references")?;
        let bin = axml::encode(&text, a.read("AndroidManifest.xml").as_deref(), &names)
            .map_err(|e| format!("{m}: {e}"))?;
        edits.insert("AndroidManifest.xml".into(), Some(bin));
    }
    if !strings.is_empty() {
        let table = export::open(&src)?
            .read("resources.arsc")
            .ok_or("no resources.arsc")?;
        edits.insert(
            "resources.arsc".into(),
            Some(arsc::set_strings(&table, &strings)?),
        );
    }
    if !smali_srcs.is_empty() {
        let changed = smali::inject(&src, &smali_srcs, &mut edits)?;
        eprintln!(
            "assembled {} smali class(es) into {}",
            smali_srcs.len(),
            changed.join(", ")
        );
    }
    let (apk, cd) = rebuild::rebuild(&src, &edits)?;
    let signed = match &key {
        Some(key) => apksign::sign(apk, cd, key)?,
        None => apk,
    };
    std::fs::write(output, &signed).map_err(|e| format!("{output}: {e}"))?;
    eprintln!(
        "wrote {output} ({} bytes, {} edits) in {:.1}s",
        signed.len(),
        edits.len(),
        t.elapsed().as_secs_f64()
    );
    if key.is_some() {
        for l in apksign::verify(output)? {
            eprintln!("{l}");
        }
    }
    Ok(())
}

#[cfg(not(feature = "gui"))]
mod gui {
    /// Printed when this build is asked for a window it cannot open. The
    /// static Linux build (musl) is compiled without the viewer on purpose:
    /// a fully static binary cannot dlopen() the desktop's X11/Wayland/GL
    /// libraries, so no window is possible there. Spell the limitation out
    /// and point at the builds that ship the viewer.
    pub fn no_viewer_note() {
        eprintln!(
            "apkscope: no viewer in this build (compiled without the `gui` feature).\n\
             On Linux this is the static musl binary (apkscope_linux_musl_x86_64/aarch64),\n\
             for servers, containers and musl distros: a fully static binary cannot load\n\
             the desktop's X11/Wayland/GL libraries at runtime, so it cannot open a\n\
             window. The viewer ships in the Windows exe, the glibc Linux binaries\n\
             (apkscope_linux_x86_64/aarch64) and the .deb/AppImage packages; on macOS,\n\
             build from source with `cargo build --release`.\n\
             Every other command works - see `apkscope --help` for the list."
        );
    }

    pub fn run(_: Option<&str>, _: bool) -> Result<(), String> {
        no_viewer_note();
        Err("the viewer is not available in this build".into())
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let parsed = (|| -> Result<(bool, Out), String> {
        let deobf = !flag(&mut args, "--no-deobf");
        if flag(&mut args, "--no-cache") {
            session::CACHE_WRITE.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(n) = num(opt(&mut args, &["--threads", "--thread"])?, "--threads")? {
            session::THREADS.store(n, std::sync::atomic::Ordering::Relaxed);
        }
        // `search` has its own --out and --format
        let search = args.get(1).is_some_and(|c| c == "search");
        let file = if search {
            None
        } else {
            opt(&mut args, &["-o", "--out"])?
        };
        let json = !search && flag(&mut args, "--json");
        let limit = num(opt(&mut args, &["--limit"])?, "--limit")?.unwrap_or(usize::MAX);
        let offset = num(opt(&mut args, &["--offset"])?, "--offset")?.unwrap_or(0);
        Ok((
            deobf,
            Out {
                file,
                json,
                limit,
                offset,
            },
        ))
    })();
    let deobf = match parsed {
        Ok((d, o)) => {
            let _ = OUT.set(o);
            d
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    // No command given and this build has no viewer: nothing can open, so
    // explain the limitation and show what this build can do instead of
    // failing on a stub. (The viewer build opens its window here.)
    #[cfg(not(feature = "gui"))]
    if args.len() < 2 {
        gui::no_viewer_note();
        eprintln!();
        return usage();
    }
    let rest: Vec<String> = args.get(2..).unwrap_or_default().to_vec();
    let result = match args.get(1..).unwrap_or_default() {
        [cmd, ..] if cmd == "classes" || cmd == "listclass" => (|| {
            let mut a = rest.clone();
            let prefix = opt(&mut a, &["--prefix"])?.unwrap_or_default();
            let (app, raw) = (flag(&mut a, "--app-only"), flag(&mut a, "--desc"));
            let [apk] = a.as_slice() else {
                return Err("usage: apkscope classes <apk> [--prefix P]".into());
            };
            let o = out_opts();
            if o.json || o.offset > 0 || o.limit != usize::MAX {
                return emit_list(&quick::classes(apk, &prefix, app, raw)?);
            }
            emit_stream(
                |sink| quick::classes_each(apk, &prefix, app, raw, sink),
                |w, names: Vec<String>| {
                    names.iter().try_for_each(|n| {
                        w.write_all(n.as_bytes()).and_then(|_| w.write_all(b"\n"))
                    })
                },
            )
        })(),
        [cmd, ..] if cmd == "methods" || cmd == "listmethod" => (|| {
            let mut a = rest.clone();
            let class = opt(&mut a, &["--class"])?;
            let filter = opt(&mut a, &["--filter", "-f"])?.unwrap_or_default();
            let [apk] = a.as_slice() else {
                return Err("usage: apkscope methods <apk> [--class C] [--filter F]".into());
            };
            let o = out_opts();
            if o.json || o.offset > 0 || o.limit != usize::MAX {
                let texts = quick::methods(apk, class.as_deref(), &filter)?;
                emit_list(
                    &texts
                        .iter()
                        .flat_map(|t| t.lines().map(str::to_string))
                        .collect::<Vec<_>>(),
                )
            } else {
                emit_stream(
                    |sink| quick::methods_each(apk, class.as_deref(), &filter, sink),
                    |w, t: String| w.write_all(t.as_bytes()),
                )
            }
        })(),
        [cmd, ..] if cmd == "strings" || cmd == "liststring" => (|| {
            let mut a = rest.clone();
            let filter = opt(&mut a, &["--filter", "-f"])?.unwrap_or_default();
            let [apk] = a.as_slice() else {
                return Err("usage: apkscope strings <apk> [--filter F]".into());
            };
            emit_list(&quick::strings(apk, &filter)?)
        })(),
        [cmd, ..] if cmd == "refs" || cmd == "findrefs" => run_big_stack(|| refs_cmd(rest.clone())),
        [cmd, ..] if cmd == "decompile" || cmd == "getclass" => {
            run_big_stack(|| decompile_cmd(rest.clone(), deobf))
        }
        [cmd, ..] if cmd == "diff" => (|| {
            let mut a = rest.clone();
            let strings = flag(&mut a, "--strings");
            let [old, new] = a.as_slice() else {
                return Err("usage: apkscope diff <old.apk> <new.apk> [--strings]".into());
            };
            emit_list(&quick::diff(old, new, strings)?)
        })(),
        [cmd, path] if cmd == "manifest" || cmd == "getmanifest" => manifest_cmd(path),
        [cmd, apk, class, rest @ ..]
            if cmd == "callers" || cmd == "callgraph" || cmd == "access" =>
        {
            run_big_stack(|| {
                let mut rest = rest.to_vec();
                let depth = num(opt(&mut rest, &["--depth"])?, "--depth")?.unwrap_or(3);
                let st = load(apk, false)?;
                let name = rest.first().map(String::as_str);
                match cmd.as_str() {
                    "callers" => emit_list(
                        &re::callers(&st.dexes, class, name)
                            .into_iter()
                            .map(|(f, t)| format!("{f}  ->  {t}"))
                            .collect::<Vec<_>>(),
                    ),
                    "access" => {
                        let field = name.ok_or("usage: apkscope access <apk> <Class> <field>")?;
                        emit_list(
                            &re::field_access(&st.dexes, class, field)
                                .into_iter()
                                .map(|(m, f, w)| {
                                    format!("{}  {m}  ({f})", if w { "write" } else { "read " })
                                })
                                .collect::<Vec<_>>(),
                        )
                    }
                    _ => emit(&re::call_graph(&st.dexes, class, name, depth)),
                }
            })
        }
        [cmd, apk, class, rest @ ..] if cmd == "frida" => {
            let mut rest = rest.to_vec();
            let live = flag(&mut rest, "--live");
            load(apk, false).and_then(|st| {
                emit(&re::frida(
                    &st.dexes,
                    class,
                    rest.first().map(String::as_str),
                    live,
                )?)
            })
        }
        [cmd, rest @ ..] if cmd == "dump" => (|| {
            let mut a = rest.to_vec();
            let attach = flag(&mut a, "--attach");
            let device = opt(&mut a, &["--device", "-D"])?;
            let wait = num(opt(&mut a, &["--wait"])?, "--wait")?.unwrap_or(15) as u64;
            let more = num(opt(&mut a, &["--scripts-for"])?, "--scripts-for")?.unwrap_or(15) as u64;
            let [app, out] = a.as_slice() else {
                return Err("usage: apkscope dump <apk|package> <out-dir> [--wait S] [--scripts-for S] [--attach] [--device ID]".into());
            };
            let target = if std::path::Path::new(app).is_file() {
                Some(quick::package(app))
                    .filter(|p| !p.is_empty())
                    .ok_or("no package name in the APK's manifest")?
            } else {
                app.clone()
            };
            eprintln!(
                "dumping {target}: waiting {wait} s for it to unpack, then {more} s more for scripts. Use the app meanwhile."
            );
            let files = live::dump(
                &target,
                attach,
                device.as_deref(),
                std::path::Path::new(out),
                wait,
                more,
            )?;
            if !files.iter().any(|f| f.starts_with("dex/")) {
                eprintln!("no DEX found in memory");
            }
            emit_list(&files)
        })(),
        [cmd, ..] if cmd == "il2cpp" => il2cpp_cmd(rest.clone()),
        [cmd, ..] if cmd == "flutter" => (|| {
            let mut a = rest.clone();
            let abi = opt(&mut a, &["--abi"])?;
            let dump = flag(&mut a, "--strings");
            let api = flag(&mut a, "--api");
            let [apk] = a.as_slice() else {
                return Err(
                    "usage: apkscope flutter <apk> [--abi NAME] [--api] [--strings]".into(),
                );
            };
            emit_list(&quick::flutter(apk, abi.as_deref(), dump, api)?)
        })(),
        [cmd, apk] if cmd == "scan" => {
            run_big_stack(|| load(apk, deobf).and_then(|st| emit(&re::scan(&st))))
        }
        [cmd, apk] if cmd == "decrypt" => load(apk, false).and_then(|st| {
            emit_list(
                &re::decrypt_all(&st.dexes)
                    .into_iter()
                    .map(|(at, f, p)| format!("{at}  {f}  =>  {}", session::json_str(&p)))
                    .collect::<Vec<_>>(),
            )
        }),
        [cmd, apk, target, text @ ..] if cmd == "rename" || cmd == "comment" => (|| {
            let mut n = re::load_notes(apk)?;
            let text = text.join(" ");
            let key = match target.split_once('#').or_else(|| target.split_once("->")) {
                Some((c, m)) => format!("{}->{m}", re::desc(c)),
                None => re::desc(target),
            };
            re::apply_note(&mut n, &key, cmd == "comment", &text);
            re::save_notes(apk, &n)?;
            eprintln!("saved {} ({key})", re::project_file(apk).display());
            Ok(())
        })(),
        [cmd, apk, dir, file] if cmd == "map" => (|| {
            let mut n = re::load_notes(apk)?;
            match dir.as_str() {
                "export" => {
                    let st = load(apk, false)?;
                    std::fs::write(
                        file,
                        re::export_mapping(&st.dexes, &n, !file.ends_with(".jobf")),
                    )
                    .map_err(|e| e.to_string())?;
                    eprintln!("wrote {file}");
                }
                "import" => {
                    let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
                    let k = re::import_mapping(&text, &mut n);
                    re::save_notes(apk, &n)?;
                    eprintln!(
                        "{k} names imported into {}",
                        re::project_file(apk).display()
                    );
                }
                _ => return Err("usage: apkscope map <apk> export|import <file>".into()),
            }
            Ok(())
        })(),
        [cmd, path] if cmd == "info" => {
            load(path, deobf).and_then(|st| emit(&session::overview(&st)))
        }
        [cmd, path, res] if cmd == "resource" => load(path, deobf).and_then(|st| {
            emit(
                &st.target_text(res)
                    .ok_or_else(|| format!("{res} not found"))?,
            )
        }),
        [cmd] if cmd == "mcp" => mcp::serve(),
        [cmd, path] if cmd == "worker" => run_big_stack(|| mcp::worker(path, deobf)),
        [cmd, path] if cmd == "disasm" => disasm(path, None),
        [cmd, path, class] if cmd == "disasm" => disasm(path, Some(class)),
        [cmd, path, class] if cmd == "smali" => {
            smali::export_class(path, class).and_then(|t| emit(&t))
        }
        [cmd, path, out] if cmd == "export" => export_cmd(path, out, deobf),
        [cmd, ..] if cmd == "search" => run_big_stack(|| search_cmd(&rest, deobf)),
        [cmd, ..] if cmd == "build" => build_cmd(&rest),
        [cmd, path] if cmd == "verify" => {
            apksign::verify(path).map(|lines| lines.iter().for_each(|l| println!("{l}")))
        }
        [cmd] if cmd == "--version" || cmd == "-V" => {
            println!("apkscope {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [] => {
            hide_console();
            gui::run(None, deobf)
        }
        [cmd] if cmd == "gui" => gui::run(None, deobf),
        [cmd, path] if cmd == "gui" => gui::run(Some(path), deobf),
        // double-clicking a file associated with apkscope
        [path]
            if session::INPUTS
                .iter()
                .any(|e| path.to_lowercase().ends_with(&format!(".{e}"))) =>
        {
            hide_console();
            gui::run(Some(path), deobf)
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
