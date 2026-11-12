//! MCP server over stdio (JSON-RPC 2.0, one message per line). Stateless tools run the CLI in
//! a fresh process, so they behave exactly like the command line and many APKs can be open at
//! once. Tools called with `cache: true` go to a warm worker per APK that keeps the session
//! (reference index, disk code cache, whole-APK deobfuscation) loaded between calls.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use serde_json::{Value, json};

use crate::live::Live;

const PROTOCOL: &str = "2025-06-18";
/// default cap on a tool's text, so one call can't flood an agent's context
const MAX_CHARS: usize = 60_000;
/// warm sessions kept at once; a big APK's session holds about 1 GB
const MAX_WORKERS: usize = 4;
/// a warm session unused this long exits and frees its memory; the next request restarts it
const IDLE_SECS: u64 = 600;

fn tools() -> Value {
    let apk = json!({"type": "string", "description": "Absolute path to the .apk, .jar, .apks, .xapk or .dex file"});
    let page = |props: Value| {
        let mut p = props;
        p["limit"] = json!({"type": "integer", "description": "At most this many results"});
        p["offset"] = json!({"type": "integer", "description": "Skip this many results (paging)"});
        p["max_chars"] = json!({"type": "integer", "description": "Cut the reply after this many characters (default 60000)"});
        p
    };
    let cache = json!({"type": "boolean", "description": "Default true: answer from a warm session of this APK kept in memory (milliseconds per call, whole-APK deobfuscation, disk code cache). While it loads in the background, calls are answered statelessly, so the first call is never slower. false = always a stateless one-shot process."});
    json!([
        {"name": "list_classes", "description": "List classes of an APK (Java names). Stateless, parallel, no cache.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": page(json!({"apk_path": apk,
            "prefix": {"type": "string", "description": "Only classes under this package / name prefix"},
            "app_only": {"type": "boolean", "description": "Skip known SDKs and libraries (androidx, okhttp, gms...)"},
            "descriptors": {"type": "boolean", "description": "Print Lcom/a/B; descriptors"}}))}},
        {"name": "list_methods", "description": "List defined methods as Lcls;->name(params)Ret, without decompiling.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": page(json!({"apk_path": apk,
            "class_name": {"type": "string", "description": "Exact class (dotted name or descriptor)"},
            "filter": {"type": "string", "description": "Case-insensitive method-name substring"}}))}},
        {"name": "list_strings", "description": "String-pool values of all DEX files, including unreferenced ones.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": page(json!({"apk_path": apk,
            "filter": {"type": "string", "description": "Case-insensitive substring"}}))}},
        {"name": "find_refs", "description": "Cross-references from bytecode: which methods use a string / type / method / field, or which classes extend / implement a type. Optionally returns the decompiled Java lines that mention it.",
         "inputSchema": {"type": "object", "required": ["apk_path", "kind", "query"], "properties": page(json!({"apk_path": apk,
            "kind": {"type": "string", "enum": ["string", "type", "method", "field", "extends", "impl"]},
            "query": {"type": "string", "description": "String text (substring), type name, or member name (exact unless regex)"},
            "class_name": {"type": "string", "description": "Owner class for method / field"},
            "fuzzy_class": {"type": "boolean", "description": "class_name is a descriptor substring"},
            "regex": {"type": "boolean"}, "ignore_case": {"type": "boolean"},
            "with_java": {"type": "boolean", "description": "Also return the decompiled Java lines of each referencing class that mention the query"},
            "json": {"type": "boolean", "description": "Return a JSON array instead of text lines"},
            "cache": cache}))}},
        {"name": "decompile", "description": "Decompile one or more classes to readable Java (or Smali), or just one method of them. Stateless by default (only the needed DEX files are touched).",
         "inputSchema": {"type": "object", "required": ["apk_path", "classes"], "properties": page(json!({"apk_path": apk,
            "classes": {"type": "array", "items": {"type": "string"}, "description": "Class names, e.g. com.example.Main"},
            "method": {"type": "string", "description": "Only return the method(s) with this name"},
            "smali": {"type": "boolean"}, "cache": cache,
            "deobfuscate": {"type": "boolean", "description": "Rename obfuscated names (default true)"}}))}},
        {"name": "search", "description": "Search decompiled code and resources. Scopes: code (decompiled Java), string (literals), usages (xref by name), class, res (XML resources + manifest), urls (endpoints, also in native libs), http (Retrofit / OkHttp / HttpURLConnection calls), secrets (API keys, tokens). Uses the reference index so only classes that can match get decompiled.",
         "inputSchema": {"type": "object", "required": ["apk_path", "scope"], "properties": page(json!({"apk_path": apk,
            "scope": {"type": "string", "enum": ["code", "string", "usages", "class", "res", "urls", "http", "secrets"]},
            "query": {"type": "string", "description": "Optional for urls / http / secrets"},
            "regex": {"type": "boolean"}, "case_sensitive": {"type": "boolean"},
            "app_only": {"type": "boolean", "description": "Skip known SDKs and libraries"},
            "context": {"type": "integer", "description": "Lines of code around each hit"},
            "whole_method": {"type": "boolean", "description": "Return the whole enclosing method of each hit"},
            "format": {"type": "string", "enum": ["txt", "json", "csv"]},
            "cache": {"type": "boolean", "description": "Keep the session warm (default true). false = one-shot process that neither reads nor writes the code cache."}}))}},
        {"name": "apk_info", "description": "Security overview: package, versions, SDK levels, permissions, exported components, deep links, native libraries, DEX/class counts.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": {"apk_path": apk, "cache": cache}}},
        {"name": "get_manifest", "description": "Decoded AndroidManifest.xml with resource names.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": {"apk_path": apk}}},
        {"name": "get_resource", "description": "Decoded resource (res/values/strings.xml, res/layout/x.xml...) or a native library report (lib/arm64-v8a/x.so: JNI exports, strings).",
         "inputSchema": {"type": "object", "required": ["apk_path", "path"], "properties": {"apk_path": apk, "path": {"type": "string"}, "cache": cache}}},
        {"name": "export_project", "description": "Write a full Gradle project (all classes decompiled on all cores, decoded resources, assets, native libs) to a directory.",
         "inputSchema": {"type": "object", "required": ["apk_path", "out_dir"], "properties": {"apk_path": apk, "out_dir": {"type": "string"}}}},
        {"name": "verify_signature", "description": "Check the APK's v1 / v2 / v3 signatures and print the signer certificate.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": {"apk_path": apk}}},
        {"name": "diff_apks", "description": "What changed between two versions of an app: added / removed classes, methods of classes both have, permissions, and optionally strings (new URLs, keys). Lines start with + or -.",
         "inputSchema": {"type": "object", "required": ["apk_path", "new_apk_path"], "properties": {"apk_path": apk, "new_apk_path": {"type": "string", "description": "the newer APK"}, "strings": {"type": "boolean", "description": "also diff string-pool values"}, "limit": {"type": "integer"}, "offset": {"type": "integer"}}}},
        {"name": "callers", "description": "Methods that call a given method (or any method of a class): who invokes it, one level up.",
         "inputSchema": {"type": "object", "required": ["apk_path", "class"], "properties": {"apk_path": apk, "class": {"type": "string", "description": "Java class name or descriptor"}, "method": {"type": "string", "description": "Method name; omit for all methods of the class"}}}},
        {"name": "call_graph", "description": "Callers of a method plus the tree of methods it calls, down to a given depth.",
         "inputSchema": {"type": "object", "required": ["apk_path", "class"], "properties": {"apk_path": apk, "class": {"type": "string"}, "method": {"type": "string"}, "depth": {"type": "integer", "description": "Callee-tree depth (default 3)"}}}},
        {"name": "field_access", "description": "Where a field is read or written: each site marked read / write, with the accessing method.",
         "inputSchema": {"type": "object", "required": ["apk_path", "class", "field"], "properties": {"apk_path": apk, "class": {"type": "string"}, "field": {"type": "string", "description": "Field name"}}}},
        {"name": "decrypt_strings", "description": "Run string-decryptor methods on their constant arguments and report the recovered plaintext (the same values shown inline as /* = \"...\" */ in decompiled code).",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": {"apk_path": apk}}},
        {"name": "scan", "description": "One-shot security scan as a single report: endpoints / URLs, embedded secrets (API keys, tokens) and HTTP-client calls across code, resources and native libraries.",
         "inputSchema": {"type": "object", "required": ["apk_path"], "properties": {"apk_path": apk}}},
        {"name": "frida_start", "description": "Start a live Frida session on a device (needs frida-tools on this machine and frida-server on the device). Spawns the app (or attaches to it), then hooks are added with frida_hook and their calls read with frida_events. One session at a time; starting again replaces it.",
         "inputSchema": {"type": "object", "properties": {"apk_path": {"type": "string", "description": "APK to take the package name from"}, "package": {"type": "string", "description": "app id, instead of apk_path"}, "attach": {"type": "boolean", "description": "attach to the running app instead of spawning it"}, "device": {"type": "string", "description": "frida device id (default: USB)"}}}},
        {"name": "frida_hook", "description": "Hook every overload of a method (or all methods of a class) in the live session: each call logs its arguments, return value or exception. Takes effect at once, no restart.",
         "inputSchema": {"type": "object", "required": ["apk_path", "class"], "properties": {"apk_path": apk, "class": {"type": "string", "description": "Java class name or descriptor"}, "method": {"type": "string", "description": "method name (omit for all methods, <init> for constructors)"}}}},
        {"name": "frida_script", "description": "Load your own Frida JavaScript into the live session under a name (same name replaces it). Wrap Java code in Java.perform; call __ev(kind, name, value) to report events that frida_events shows; console.log lines show as kind log.",
         "inputSchema": {"type": "object", "required": ["name", "js"], "properties": {"name": {"type": "string"}, "js": {"type": "string"}}}},
        {"name": "frida_unhook", "description": "Remove a hook or script: a class (all its methods), Class#method, a script name, or * for everything.",
         "inputSchema": {"type": "object", "required": ["key"], "properties": {"key": {"type": "string"}}}},
        {"name": "frida_events", "description": "What the hooks saw: one line per event (#seq thread kind method value), kinds call / ret / throw / error / log. Pass the returned next_since back to get only newer events; summary: true counts calls per method instead.",
         "inputSchema": {"type": "object", "properties": {"since": {"type": "integer"}, "filter": {"type": "string", "description": "substring of method or value"}, "limit": {"type": "integer", "description": "newest events to return (default 200)"}, "summary": {"type": "boolean"}}}},
        {"name": "frida_stop", "description": "End the live session (kills frida; a spawned app keeps running).",
         "inputSchema": {"type": "object", "properties": {}}}
    ])
}

/// A warm session process for one APK. Its replies arrive through `rx`; the first one says
/// the session has loaded.
struct Worker {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    ready: bool,
    used: Instant,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

/// Run the CLI once; returns (stdout + stderr notes, success).
fn run_cli(args: &[String]) -> Result<(String, bool), String> {
    let out = Command::new(exe()?)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text += &format!("\n[{}]\n", err.trim());
    }
    Ok((text, out.status.success()))
}

/// Ask the APK's warm session; until it has loaded, answer with the stateless `fallback`
/// command instead (requests without one wait for the load).
fn ask_worker(
    workers: &mut HashMap<String, Worker>,
    apk: &str,
    deobf: bool,
    req: &[String],
    fallback: Option<Vec<String>>,
) -> Result<(String, bool), String> {
    let key = format!("{apk}\0{deobf}");
    // a worker that exited on its own (idle timeout) is started again
    if workers
        .get_mut(&key)
        .is_some_and(|w| !matches!(w.child.try_wait(), Ok(None)))
    {
        workers.remove(&key);
    }
    if !workers.contains_key(&key) {
        if workers.len() >= MAX_WORKERS {
            let old = workers.iter().min_by_key(|w| w.1.used).map(|w| w.0.clone());
            old.map(|k| workers.remove(&k));
        }
        let mut cmd = Command::new(exe()?);
        cmd.arg("worker").arg(apk);
        if !deobf {
            cmd.arg("--no-deobf");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("no worker stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no worker stdout")?);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        workers.insert(
            key.clone(),
            Worker {
                child,
                stdin,
                rx,
                ready: false,
                used: Instant::now(),
            },
        );
    }
    let w = workers.get_mut(&key).unwrap();
    w.used = Instant::now();
    if !w.ready {
        let first = match &fallback {
            Some(_) => w.rx.try_recv(),
            None => w.rx.recv().map_err(|_| TryRecvError::Disconnected),
        };
        match first {
            Ok(_) => w.ready = true,
            Err(TryRecvError::Empty) => return run_cli(fallback.as_deref().unwrap_or_default()),
            Err(TryRecvError::Disconnected) => {
                workers.remove(&key);
                return match fallback {
                    Some(f) => run_cli(&f),
                    None => Err("the session could not load this APK".into()),
                };
            }
        }
    }
    let sent = writeln!(w.stdin, "{}", json!(req)).and_then(|_| w.stdin.flush());
    let line = match sent.ok().and_then(|_| w.rx.recv().ok()) {
        Some(l) => l,
        None => {
            workers.remove(&key);
            return Err("the session worker exited (APK unreadable?)".into());
        }
    };
    let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    // the full session is still loading: answer this one statelessly
    if v["busy"].as_bool() == Some(true) {
        return run_cli(fallback.as_deref().unwrap_or_default());
    }
    Ok((
        v["text"].as_str().unwrap_or_default().to_string(),
        v["ok"].as_bool().unwrap_or(false),
    ))
}

fn s(a: &Value, k: &str) -> Option<String> {
    a.get(k)
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

fn b(a: &Value, k: &str) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn n(a: &Value, k: &str) -> Option<u64> {
    a.get(k).and_then(Value::as_u64)
}

/// `cache` defaults to on
fn cached(a: &Value) -> bool {
    a.get("cache").and_then(Value::as_bool).unwrap_or(true)
}

fn call(
    workers: &mut HashMap<String, Worker>,
    name: &str,
    a: &Value,
) -> Result<(String, bool), String> {
    let apk = s(a, "apk_path").ok_or("apk_path is required")?;
    let mut args: Vec<String> = Vec::new();
    let push_opt = |args: &mut Vec<String>, flag: &str, v: Option<String>| {
        if let Some(v) = v {
            args.push(flag.into());
            args.push(v);
        }
    };
    let paging = |args: &mut Vec<String>| {
        for k in ["limit", "offset"] {
            if let Some(v) = n(a, k) {
                args.push(format!("--{k}"));
                args.push(v.to_string());
            }
        }
    };
    let deobf = a
        .get("deobfuscate")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    match name {
        "list_classes" => {
            args.extend(["classes".into(), apk]);
            push_opt(&mut args, "--prefix", s(a, "prefix"));
            if b(a, "app_only") {
                args.push("--app-only".into())
            }
            if b(a, "descriptors") {
                args.push("--desc".into())
            }
            paging(&mut args);
            run_cli(&args)
        }
        "list_methods" => {
            args.extend(["methods".into(), apk]);
            push_opt(&mut args, "--class", s(a, "class_name"));
            push_opt(&mut args, "--filter", s(a, "filter"));
            paging(&mut args);
            run_cli(&args)
        }
        "list_strings" => {
            args.extend(["strings".into(), apk]);
            push_opt(&mut args, "--filter", s(a, "filter"));
            paging(&mut args);
            run_cli(&args)
        }
        "find_refs" => {
            let mut req = vec![
                "refs".to_string(),
                s(a, "kind").ok_or("kind is required")?,
                s(a, "query").ok_or("query is required")?,
            ];
            push_opt(&mut req, "--class", s(a, "class_name"));
            for (k, f) in [
                ("fuzzy_class", "--fuzzy-class"),
                ("regex", "--regex"),
                ("ignore_case", "--ignore-case"),
                ("with_java", "--java"),
                ("json", "--json"),
            ] {
                if b(a, k) {
                    req.push(f.into())
                }
            }
            paging(&mut req);
            args.extend(["refs".into(), apk.clone()]);
            args.extend(req.iter().skip(1).cloned());
            if cached(a) {
                return ask_worker(workers, &apk, deobf, &req, Some(args));
            }
            run_cli(&args)
        }
        "decompile" => {
            let classes: Vec<String> = match a.get("classes") {
                Some(Value::Array(v)) => v
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                Some(Value::String(c)) => vec![c.clone()],
                _ => vec![],
            };
            if classes.is_empty() {
                return Err("classes is required (one or more class names)".into());
            }
            let mut opts = Vec::new();
            push_opt(&mut opts, "--method", s(a, "method"));
            if b(a, "smali") {
                opts.push("--smali".into())
            }
            let mut req = vec!["decompile".to_string()];
            req.extend(classes.iter().cloned());
            req.extend(opts.iter().cloned());
            args.extend(["decompile".into(), apk.clone()]);
            args.extend(classes);
            args.extend(opts);
            if !deobf {
                args.push("--no-deobf".into())
            }
            if cached(a) {
                return ask_worker(workers, &apk, deobf, &req, Some(args));
            }
            run_cli(&args)
        }
        "search" => {
            let mut req = vec![
                "search".to_string(),
                s(a, "scope").ok_or("scope is required")?,
            ];
            req.extend(s(a, "query"));
            for (k, f) in [
                ("regex", "--regex"),
                ("case_sensitive", "--case"),
                ("app_only", "--app-only"),
                ("whole_method", "--method"),
            ] {
                if b(a, k) {
                    req.push(f.into())
                }
            }
            push_opt(
                &mut req,
                "--context",
                n(a, "context").map(|c| c.to_string()),
            );
            push_opt(&mut req, "--format", s(a, "format"));
            if cached(a) {
                return ask_worker(workers, &apk, deobf, &req, None);
            }
            args.extend(["search".into(), apk]);
            args.extend(req.into_iter().skip(1));
            args.push("--no-cache".into());
            if !deobf {
                args.push("--no-deobf".into())
            }
            run_cli(&args)
        }
        "apk_info" if cached(a) => ask_worker(
            workers,
            &apk,
            deobf,
            &["info".into()],
            Some(vec!["info".into(), apk.clone()]),
        ),
        "apk_info" => run_cli(&["info".into(), apk]),
        "get_manifest" => run_cli(&["manifest".into(), apk]),
        "get_resource" => {
            let path = s(a, "path").ok_or("path is required")?;
            if cached(a) {
                return ask_worker(
                    workers,
                    &apk,
                    deobf,
                    &["resource".into(), path.clone()],
                    Some(vec!["resource".into(), apk.clone(), path]),
                );
            }
            run_cli(&["resource".into(), apk, path])
        }
        "export_project" => run_cli(&[
            "export".into(),
            apk,
            s(a, "out_dir").ok_or("out_dir is required")?,
        ]),
        "verify_signature" => run_cli(&["verify".into(), apk]),
        "diff_apks" => {
            args.extend([
                "diff".into(),
                apk,
                s(a, "new_apk_path").ok_or("new_apk_path is required")?,
            ]);
            if b(a, "strings") {
                args.push("--strings".into());
            }
            paging(&mut args);
            run_cli(&args)
        }
        "callers" => {
            args.extend([
                "callers".into(),
                apk,
                s(a, "class").ok_or("class is required")?,
            ]);
            if let Some(m) = s(a, "method") {
                args.push(m)
            }
            run_cli(&args)
        }
        "call_graph" => {
            args.extend([
                "callgraph".into(),
                apk,
                s(a, "class").ok_or("class is required")?,
            ]);
            if let Some(m) = s(a, "method") {
                args.push(m)
            }
            push_opt(&mut args, "--depth", n(a, "depth").map(|d| d.to_string()));
            run_cli(&args)
        }
        "field_access" => {
            args.extend([
                "access".into(),
                apk,
                s(a, "class").ok_or("class is required")?,
                s(a, "field").ok_or("field is required")?,
            ]);
            run_cli(&args)
        }
        "decrypt_strings" => run_cli(&["decrypt".into(), apk]),
        "scan" => run_cli(&["scan".into(), apk]),
        _ => Err(format!("unknown tool {name}")),
    }
}

/// The frida_* tools, on the one live session.
fn live_call(live: &mut Option<Live>, name: &str, a: &Value) -> Result<(String, bool), String> {
    if name == "frida_start" {
        *live = None;
        let target = match (s(a, "package"), s(a, "apk_path")) {
            (Some(p), _) => p,
            (None, Some(apk)) => Some(crate::quick::package(&apk))
                .filter(|p| !p.is_empty())
                .ok_or("no package name in the APK's manifest; pass package")?,
            _ => return Err("pass apk_path or package".into()),
        };
        let l = Live::start(&target, b(a, "attach"), s(a, "device").as_deref())?;
        let text = format!("live on {target}\n{}", l.text(0, "", 50).0);
        *live = Some(l);
        return Ok((text, true));
    }
    let l = live
        .as_mut()
        .ok_or("no live session: call frida_start first")?;
    if !l.alive() && name != "frida_events" {
        return Err(format!("frida has exited:\n{}", l.text(0, "", 30).0));
    }
    match name {
        "frida_hook" => {
            let class = s(a, "class").ok_or("class is required")?;
            let method = s(a, "method");
            let mut args = vec![
                "frida".into(),
                s(a, "apk_path").ok_or("apk_path is required")?,
                class.clone(),
            ];
            args.extend(method.clone());
            args.push("--live".into());
            let (js, ok) = run_cli(&args)?;
            if !ok {
                return Ok((js, false));
            }
            let key = method.map_or(class.clone(), |m| format!("{class}#{m}"));
            l.hook(&key, &js, false)?;
            Ok((
                format!(
                    "hooked {key} ({} overloads); active: {}",
                    js.matches(".implementation").count(),
                    l.hook_keys().join(", ")
                ),
                true,
            ))
        }
        "frida_script" => {
            let key = s(a, "name").ok_or("name is required")?;
            l.hook(&key, &s(a, "js").ok_or("js is required")?, true)?;
            Ok((
                format!("loaded {key}; active: {}", l.hook_keys().join(", ")),
                true,
            ))
        }
        "frida_unhook" => {
            let n = l.unhook(&s(a, "key").ok_or("key is required")?)?;
            Ok((
                format!("removed {n}; active: {}", l.hook_keys().join(", ")),
                n > 0,
            ))
        }
        "frida_events" => {
            let since = n(a, "since").unwrap_or(0);
            let (body, next) = if b(a, "summary") {
                (l.summary(since), l.events.lock().unwrap().next)
            } else {
                l.text(
                    since,
                    &s(a, "filter").unwrap_or_default(),
                    n(a, "limit").unwrap_or(200) as usize,
                )
            };
            Ok((
                format!(
                    "next_since: {next}{}\n{body}",
                    if l.alive() {
                        ""
                    } else {
                        "  (frida has exited)"
                    }
                ),
                true,
            ))
        }
        "frida_stop" => {
            *live = None;
            Ok(("stopped".into(), true))
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

/// Cut `text` at `max` characters on a line boundary, saying how to get the rest.
fn cap(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    let cut = cut
        .rfind('\n')
        .map_or(cut.as_str(), |k| &cut[..k])
        .to_string();
    format!(
        "{cut}\n\n[truncated at {max} of {} characters: narrow the query, use limit/offset, or raise max_chars]",
        text.chars().count()
    )
}

pub fn serve() -> Result<(), String> {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut live: Option<Live> = None;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})
                );
                continue;
            }
        };
        // notifications get no reply
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default();
        let params = &msg["params"];
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "apkscope", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "apkscope analyzes Android APKs. Start with apk_info, then find_refs / list_classes / search to locate code, then decompile only the classes (or one method) you need. Calls on the same APK get faster after the first ones: a warm session loads in the background (pass cache: false to opt out)."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let max = n(&args, "max_chars").map_or(MAX_CHARS, |m| m as usize);
                let res = if name.starts_with("frida_") {
                    live_call(&mut live, name, &args)
                } else {
                    call(&mut workers, name, &args)
                };
                let (text, ok) = res.unwrap_or_else(|e| (e, false));
                Ok(json!({"content": [{"type": "text", "text": cap(text, max)}], "isError": !ok}))
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let msg = match reply {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }
    Ok(())
}

/// `apkscope worker <apk>`: keeps one APK warm and answers one JSON request per line
/// (`["refs", kind, query, flags...]`, `["search", scope, query, flags...]`,
/// `["decompile", class..., flags]`, `["info"]`, `["resource", path]`) with
/// `{"ok": bool, "text": "..."}`. The raw DEX files load first (enough for refs, well under a
/// second); the full session builds in the background, and until it is there requests that
/// need it get `{"busy": true}` (search waits for it instead).
pub fn worker(apk: &str, deobf: bool) -> Result<(), String> {
    let files = crate::quick::scan(apk, |l| {
        l.all()?;
        Ok((std::mem::take(&mut l.name), std::mem::take(&mut l.buf)))
    })?;
    let dexes: Vec<(String, crate::dex::Dex)> = files
        .iter()
        .filter_map(|(n, b)| Some((n.clone(), crate::dex::Dex::parse(b).ok()?)))
        .collect();
    let state: std::sync::Arc<std::sync::OnceLock<Result<crate::session::State, String>>> =
        Default::default();
    let (bg, path) = (state.clone(), apk.to_string());
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let _ = bg.set(crate::load(&path, deobf));
        })
        .map_err(|e| e.to_string())?;
    // an idle session holds the whole APK in memory: give it back after IDLE_SECS unused
    static LAST: AtomicU64 = AtomicU64::new(0);
    let start = Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if start.elapsed().as_secs().saturating_sub(LAST.load(Relaxed)) > IDLE_SECS {
                std::process::exit(0);
            }
        }
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", json!({"ready": true}));
    let _ = out.flush();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        // never idle while a request runs, however long it takes
        LAST.store(u64::MAX, Relaxed);
        let req: Vec<String> = serde_json::from_str(&line).unwrap_or_default();
        let kind = req.first().map(String::as_str).unwrap_or_default();
        let st = match kind {
            "refs" => None,
            "search" => Some(state.wait()),
            _ => match state.get() {
                Some(s) => Some(s),
                None => {
                    LAST.store(start.elapsed().as_secs(), Relaxed);
                    let _ = writeln!(out, "{}", json!({"ok": false, "busy": true}));
                    let _ = out.flush();
                    continue;
                }
            },
        };
        let res: Result<String, String> = (|| {
            if kind == "refs" {
                let mut r = req[1..].to_vec();
                let json = crate::flag(&mut r, "--json");
                let limit =
                    crate::num(crate::opt(&mut r, &["--limit"])?, "--limit")?.unwrap_or(usize::MAX);
                let offset =
                    crate::num(crate::opt(&mut r, &["--offset"])?, "--offset")?.unwrap_or(0);
                let (q, java) = crate::refs_query(r)?;
                let hits = crate::quick::refs_loaded(&dexes, &q, offset.saturating_add(limit))?;
                let hits = &hits[offset.min(hits.len())..];
                // Java context needs the session's decompiler (whole-APK names, disk cache)
                let st = if java {
                    Some(state.wait().as_ref()?)
                } else {
                    None
                };
                let java_of = |owners: &[String]| -> Vec<(String, Option<String>)> {
                    owners
                        .iter()
                        .map(|o| {
                            (
                                o.clone(),
                                st.and_then(|st| {
                                    st.class_text(
                                        &format!(
                                            "{};",
                                            o.trim_end_matches(';').split('$').next().unwrap_or(o)
                                        ),
                                        false,
                                    )
                                }),
                            )
                        })
                        .collect()
                };
                let text =
                    crate::refs_text(&q, hits, json, if java { Some(&java_of) } else { None })?;
                return Ok(format!("{text}[{} refs]", hits.len()));
            }
            let st = st.expect("needs the session").as_ref()?;
            match kind {
                "search" => {
                    let a = crate::search_args(&req[1..])?;
                    let (text, hits, note) = crate::search_on(st, &a, &a.pos)?;
                    Ok(format!("{text}\n[{hits} hits. {note}]"))
                }
                "decompile" => {
                    let mut r = req[1..].to_vec();
                    let method = crate::opt(&mut r, &["--method"])?;
                    let smali = crate::flag(&mut r, "--smali");
                    let mut text = String::new();
                    for c in &r {
                        let t = st
                            .class_text(c, smali)
                            .ok_or_else(|| format!("class {c} not found"))?;
                        match &method {
                            Some(m) => {
                                text += &crate::quick::method_of(&t, m)
                                    .ok_or_else(|| format!("method {m} not found in {c}"))?
                            }
                            None => text += &t,
                        }
                        text.push('\n');
                    }
                    Ok(text)
                }
                "info" => Ok(crate::session::overview(st)),
                "resource" => st
                    .target_text(req.get(1).map_or("", |s| s))
                    .ok_or_else(|| "resource not found".to_string()),
                _ => Err(format!("unknown request {line}")),
            }
        })();
        let (ok, text) = match res {
            Ok(t) => (true, t),
            Err(e) => (false, e),
        };
        LAST.store(start.elapsed().as_secs(), Relaxed);
        let _ = writeln!(out, "{}", json!({"ok": ok, "text": text}));
        let _ = out.flush();
    }
    Ok(())
}
