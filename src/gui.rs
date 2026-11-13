//! `apkscope gui`: native window (egui). The APK loads on a background thread, then the
//! reference index builds in the background; classes are decompiled when opened.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextFormat, text::LayoutJob,
};

use crate::{apksign, arsc, cache, rebuild, smali};

mod live_ui;
use crate::re::{Line, Link};
use crate::session::{self, Format, Hit, Matcher, Origin, Progress, Scope, State};

// ---------- settings ----------

#[derive(Clone, Copy, PartialEq)]
enum Theme {
    Dark,
    Midnight,
    Light,
    Solarized,
}

impl Theme {
    const ALL: [Theme; 4] = [Theme::Dark, Theme::Midnight, Theme::Light, Theme::Solarized];

    fn name(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Midnight => "Midnight",
            Theme::Light => "Light",
            Theme::Solarized => "Solarized light",
        }
    }

    fn dark(self) -> bool {
        matches!(self, Theme::Dark | Theme::Midnight)
    }

    /// (background, panel, accent, text) and code colours
    fn palette(self) -> Palette {
        let c = Color32::from_rgb;
        match self {
            Theme::Dark => Palette {
                bg: c(24, 25, 28),
                panel: c(32, 33, 37),
                accent: c(77, 144, 254),
                text: c(220, 222, 226),
                dim: c(140, 144, 150),
                code: [
                    c(212, 212, 212),
                    c(106, 153, 85),
                    c(206, 145, 120),
                    c(86, 156, 214),
                    c(181, 206, 168),
                    c(220, 160, 90),
                ],
                mark: c(78, 66, 22),
            },
            Theme::Midnight => Palette {
                bg: c(13, 17, 23),
                panel: c(22, 27, 34),
                accent: c(88, 166, 255),
                text: c(201, 209, 217),
                dim: c(125, 133, 144),
                code: [
                    c(201, 209, 217),
                    c(139, 148, 158),
                    c(165, 214, 255),
                    c(255, 123, 114),
                    c(121, 192, 255),
                    c(255, 166, 87),
                ],
                mark: c(56, 52, 20),
            },
            Theme::Light => Palette {
                bg: c(250, 250, 251),
                panel: c(240, 241, 244),
                accent: c(37, 99, 235),
                text: c(28, 30, 34),
                dim: c(100, 104, 112),
                code: [
                    c(30, 30, 30),
                    c(0, 128, 0),
                    c(163, 21, 21),
                    c(0, 0, 220),
                    c(9, 134, 88),
                    c(170, 90, 0),
                ],
                mark: c(255, 236, 150),
            },
            Theme::Solarized => Palette {
                bg: c(253, 246, 227),
                panel: c(238, 232, 213),
                accent: c(38, 139, 210),
                text: c(88, 110, 117),
                dim: c(147, 161, 161),
                code: [
                    c(88, 110, 117),
                    c(147, 161, 161),
                    c(42, 161, 152),
                    c(133, 153, 0),
                    c(211, 54, 130),
                    c(203, 75, 22),
                ],
                mark: c(240, 220, 160),
            },
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    accent: Color32,
    text: Color32,
    dim: Color32,
    /// plain, comment, string, keyword, number, directive/warning
    code: [Color32; 6],
    mark: Color32,
}

#[derive(Clone)]
struct Settings {
    theme: Theme,
    code_font: f32,
    ui_scale: f32,
    deobf: bool,
    threads: usize,
    use_cache: bool,
    export_ctx: usize,
    export_method: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: Theme::Dark,
            code_font: 13.0,
            ui_scale: 1.0,
            deobf: true,
            threads: 0,
            use_cache: true,
            export_ctx: 3,
            export_method: false,
        }
    }
}

impl Settings {
    fn file() -> std::path::PathBuf {
        let base = std::env::var_os("APPDATA")
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("apkscope").join("settings.txt")
    }

    fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = std::fs::read_to_string(Self::file()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "theme" => {
                    s.theme = Theme::ALL
                        .into_iter()
                        .find(|t| t.name() == v.trim())
                        .unwrap_or(s.theme)
                }
                "code_font" => s.code_font = v.trim().parse().unwrap_or(s.code_font),
                "ui_scale" => s.ui_scale = v.trim().parse().unwrap_or(s.ui_scale),
                "deobf" => s.deobf = v.trim() == "true",
                "threads" => s.threads = v.trim().parse().unwrap_or(0),
                "use_cache" => s.use_cache = v.trim() == "true",
                "export_ctx" => s.export_ctx = v.trim().parse().unwrap_or(3),
                "export_method" => s.export_method = v.trim() == "true",
                _ => {}
            }
        }
        s
    }

    fn save(&self) {
        let p = Self::file();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let text = format!(
            "theme={}\ncode_font={}\nui_scale={}\ndeobf={}\nthreads={}\nuse_cache={}\nexport_ctx={}\nexport_method={}\n",
            self.theme.name(),
            self.code_font,
            self.ui_scale,
            self.deobf,
            self.threads,
            self.use_cache,
            self.export_ctx,
            self.export_method
        );
        let _ = std::fs::write(p, text);
    }
}

fn mb(b: u64) -> String {
    match b {
        0..1_048_576 => format!("{} KB", b.div_ceil(1024)),
        _ => format!("{:.1} MB", b as f64 / 1048576.0),
    }
}

// ---------- trees ----------

/// Package tree of the class list.
#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    /// (short name, full name)
    leaves: Vec<(String, String)>,
}

fn tree<'a>(classes: impl Iterator<Item = &'a String>) -> Tree {
    let mut root = Tree::default();
    for full in classes {
        let mut node = &mut root;
        let mut parts: Vec<&str> = full.split('.').collect();
        let leaf = parts.pop().unwrap_or(full);
        for p in parts {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        node.leaves.push((leaf.to_string(), full.clone()));
    }
    root
}

/// Resource paths grouped by folder: `res/layout/main.xml` -> "res/layout" -> "main.xml".
fn res_groups(st: &State) -> BTreeMap<String, Vec<String>> {
    let mut g: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in st.texts.keys().chain(st.files.keys()) {
        let dir = p.rsplit_once('/').map_or("", |d| d.0);
        g.entry(dir.to_string()).or_default().push(p.clone());
    }
    g
}

// ---------- documents ----------

#[derive(Clone, Copy, PartialEq)]
enum View {
    Java,
    Smali,
}

#[derive(Clone, PartialEq)]
enum Open {
    Overview,
    Class(String),
    Res(String),
    Native(String),
    File(String),
    /// generated text: callers, call graph, Frida script, security scan ...
    Report(String),
}

impl Open {
    fn name(&self) -> &str {
        match self {
            Open::Overview => "Overview",
            Open::Class(c) | Open::Res(c) | Open::Native(c) | Open::File(c) | Open::Report(c) => c,
        }
    }

    fn short(&self) -> &str {
        let n = self.name();
        let sep = match self {
            Open::Class(_) => '.',
            Open::Report(_) => return n,
            _ => '/',
        };
        n.rsplit_once(sep).map_or(n, |x| x.1)
    }
}

struct Doc {
    open: Open,
    view: View,
    text: Option<Arc<String>>,
    /// per line of a report: where clicking it leads
    links: Option<Arc<Vec<Option<Link>>>>,
    /// 1-based line to scroll to and mark
    mark: Option<usize>,
    scroll_pending: bool,
    /// highlighted text and what it was built for: (background, font size x10, marked line, selected word)
    job: Option<((Color32, u32, Option<usize>, Option<String>), LayoutJob)>,
    /// identifier clicked in the code, marked everywhere in the file
    sel: Option<String>,
    find: String,
    /// manifest text being edited, when in edit mode
    editing: Option<String>,
    /// where to jump once the document's text arrives
    find_on_load: Option<Target>,
    /// line and column (0-based) the context menu was opened on
    menu_at: Option<(usize, usize)>,
    /// line actions opened from the keyboard (Shift+F10), shown in a window
    kb_menu: bool,
}

impl Doc {
    fn new(open: Open) -> Doc {
        Doc {
            open,
            view: View::Java,
            text: None,
            links: None,
            mark: None,
            scroll_pending: false,
            job: None,
            sel: None,
            find: String::new(),
            editing: None,
            find_on_load: None,
            menu_at: None,
            kb_menu: false,
        }
    }
}

/// A place inside a class's Java: a method declaration (shown name, parameter count), then
/// optionally the first line after it that contains `then` (a call, a field use).
#[derive(Clone)]
struct Target {
    decl: Option<(String, Option<usize>)>,
    then: Option<String>,
}

/// "Rename" / "Comment" dialog
struct NoteDlg {
    /// project-file key: `La/b;`, `La/b;->field`, `La/b;->method()`
    key: String,
    comment: bool,
    label: String,
    text: String,
}

/// What a click or right-click in the code asked for.
enum Act {
    Callers(String, Option<String>),
    Graph(String, Option<String>),
    Frida(String, Option<String>),
    /// hook in the running Frida session
    LiveHook(String, Option<String>),
    Access(String, String),
    Smali(String, String),
    Note(String, bool, String),
    GoTo(Link),
    /// top-level class (Java name as listed), and where in it
    OpenClass(String, Option<Target>),
    /// 1-based line of the active tab
    Line(usize),
    Usages(String),
}

fn java_res() -> &'static [regex::Regex; 3] {
    use std::sync::OnceLock;
    static RE: OnceLock<[regex::Regex; 3]> = OnceLock::new();
    RE.get_or_init(|| {
        [
            regex::Regex::new(
                r"^(\s*)(?:[\w@]+\s+)*(?:class|interface|enum|@interface)\s+([\w$]+)",
            )
            .unwrap(),
            regex::Regex::new(
                r"^(\s+)(?:[\w$<>\[\],.?@]+\s+)*?([\w$]+)\((.*)\)\s*(?:throws [^{;]+)?\s*[{;]\s*$",
            )
            .unwrap(),
            regex::Regex::new(r"^(\s+)(?:[\w$<>\[\],.?]+\s+)+([\w$]+)(?: = .*)?;(?:\s*//.*)?$")
                .unwrap(),
        ]
    })
}

const NOT_DECL: [&str; 9] = [
    "if",
    "while",
    "for",
    "switch",
    "catch",
    "synchronized",
    "return",
    "new",
    "super",
];

/// A method_re match is a declaration, not a call statement: a type or modifier comes first.
fn is_decl(l: &str, c: &regex::Captures) -> bool {
    let pre = l[..c.get(2).map_or(0, |m| m.start())].trim();
    let first = pre.split_whitespace().next().unwrap_or("");
    // a constructor without modifiers: `Name(args) {`
    let ctor = pre.is_empty()
        && c[2].starts_with(|ch: char| ch.is_uppercase())
        && l.trim_end().ends_with('{');
    (ctor || !pre.is_empty())
        && !pre.ends_with('.')
        && !NOT_DECL.contains(&&c[2])
        && !NOT_DECL.contains(&first)
        && first != "throw"
        && first != "else"
        && !l.contains(" = ")
}

fn indent(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// What sits on line `at` of decompiled Java: the chain of enclosing classes (as shown) and the
/// member declared there, as (is_method, shown name). A comment above a declaration (such as a
/// WARNING) points at that declaration.
fn member_at(text: &str, at: usize) -> Option<(Vec<String>, Option<(bool, String)>)> {
    let [class_re, method_re, field_re] = java_res();
    let lines: Vec<&str> = text.lines().collect();
    let mut at = at.min(lines.len().checked_sub(1)?);
    while at + 1 < lines.len() && {
        let t = lines[at].trim_start();
        t.starts_with("/*") || t.starts_with('*') || t.starts_with("//") || t.starts_with('@')
    } {
        at += 1;
    }
    let l = lines[at];
    let (member, own_indent) = if let Some(c) = class_re.captures(l) {
        (None, c[1].len() + 4)
    } else if let Some(c) = method_re.captures(l).filter(|c| is_decl(l, c)) {
        (Some((true, c[2].to_string())), c[1].len())
    } else if let Some(c) = field_re.captures(l).filter(|c| !NOT_DECL.contains(&&c[2])) {
        (Some((false, c[2].to_string())), c[1].len())
    } else {
        return None;
    };
    // enclosing classes, innermost last
    let mut chain = Vec::new();
    let mut want = own_indent.checked_sub(4)?;
    for k in (0..=at).rev() {
        if let Some(c) = class_re
            .captures(lines[k])
            .filter(|_| indent(lines[k]) == want)
        {
            chain.push(c[2].to_string());
            if want == 0 {
                break;
            }
            want -= 4;
        }
    }
    chain.reverse();
    (!chain.is_empty() && want == 0).then_some((chain, member))
}

/// Classes enclosing any line (a statement inside a method too), innermost last.
fn classes_around(text: &str, at: usize) -> Vec<String> {
    let class_re = &java_res()[0];
    let lines: Vec<&str> = text.lines().collect();
    let mut cur = lines.get(at).map_or(0, |l| indent(l)) + 1;
    let mut chain = Vec::new();
    for k in (0..=at.min(lines.len().saturating_sub(1))).rev() {
        if let Some(c) = class_re
            .captures(lines[k])
            .filter(|_| indent(lines[k]) < cur)
        {
            chain.push(c[2].to_string());
            cur = indent(lines[k]);
            if cur == 0 {
                break;
            }
        }
    }
    chain.reverse();
    chain
}

/// 1-based line of the declaration of method `name` (with `nargs` parameters, if given).
fn find_decl(text: &str, name: &str, nargs: Option<usize>) -> Option<usize> {
    let re = &java_res()[1];
    let mut first = None;
    for (k, l) in text.lines().enumerate() {
        let Some(c) = re.captures(l) else { continue };
        if &c[2] != name || !is_decl(l, &c) {
            continue;
        }
        let n = if c[3].trim().is_empty() {
            0
        } else {
            c[3].matches(',').count() + 1
        };
        if nargs.is_none_or(|w| w == n) {
            return Some(k + 1);
        }
        first.get_or_insert(k + 1);
    }
    first
}

/// Line to show for a target: the declaration, then the first line after it with `then`.
fn locate(text: &str, t: &Target) -> Option<usize> {
    let decl = t.decl.as_ref().and_then(|(n, a)| find_decl(text, n, *a));
    match (&t.then, decl) {
        (Some(w), Some(d)) => {
            // stay inside the method: stop at the next line indented like its declaration
            let lines: Vec<&str> = text.lines().collect();
            let ind = indent(lines[d - 1]);
            let hit = lines
                .iter()
                .enumerate()
                .skip(d)
                .take_while(|(_, l)| l.trim().is_empty() || indent(l) > ind || l.trim() == "}")
                .find(|(_, l)| l.contains(w.as_str()));
            Some(hit.map_or(d, |(k, _)| k + 1))
        }
        (Some(w), None) => find_next(text, w, 0),
        (None, d) => d,
    }
}

/// Identifier at (line, column) and what surrounds it: (word, followed by `(`, after `new `,
/// receiver before a `.`).
fn word_at(text: &str, line: usize, col: usize) -> Option<(String, bool, bool, Option<String>)> {
    let l: Vec<char> = text.lines().nth(line)?.chars().collect();
    let w = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    if !l.get(col).is_some_and(|&c| w(c)) {
        return None;
    }
    let (mut s, mut e) = (col, col);
    while s > 0 && w(l[s - 1]) {
        s -= 1;
    }
    while e < l.len() && w(l[e]) {
        e += 1;
    }
    let word: String = l[s..e].iter().collect();
    if word.chars().next()?.is_ascii_digit() {
        return None;
    }
    let call = l.get(e) == Some(&'(');
    let before: String = l[..s].iter().collect();
    let is_new = before.ends_with("new ");
    let recv = before
        .strip_suffix('.')
        .map(|b| {
            b.chars()
                .rev()
                .take_while(|&c| w(c))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        })
        .filter(|r| !r.is_empty());
    Some((word, call, is_new, recv))
}

/// Top-level class (as listed) a simple name in `text` refers to: by its import, the tab's own
/// package, or a unique name in the APK.
fn resolve_class(st: &State, text: &str, top: &str, simple: &str) -> Option<String> {
    let has = |n: &str| st.classes.binary_search_by(|c| c.0.as_str().cmp(n)).is_ok();
    let suffix = format!(".{simple};");
    if let Some(imp) = text
        .lines()
        .take_while(|l| !l.contains(" class ") && !l.contains("interface "))
        .find(|l| l.starts_with("import ") && l.ends_with(&suffix))
    {
        let n = imp.trim_start_matches("import ").trim_end_matches(';');
        if has(n) {
            return Some(n.to_string());
        }
    }
    let same = format!("{}.{simple}", top.rsplit_once('.').map_or("", |x| x.0));
    if has(&same) {
        return Some(same);
    }
    let mut it = st
        .classes
        .iter()
        .filter(|c| c.0.rsplit('.').next() == Some(simple));
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c.0.clone()),
        _ => None,
    }
}

/// Real descriptor of a class shown as the last of `chain` in tab `top`.
fn owner_desc(st: &State, top: &str, chain: &[String]) -> Option<String> {
    let &(_, d, c) = st.classes.iter().find(|x| x.0 == top)?;
    let t = st.dexes[d].class_descriptor(c).ok()?;
    Some(if chain.len() > 1 {
        crate::re::nested_class(&st.dexes, &t, chain.last()?)
    } else {
        t
    })
}

fn desc_of(st: &State, java: &str) -> Option<String> {
    let &(_, d, c) = st.classes.iter().find(|x| x.0 == java)?;
    st.dexes[d].class_descriptor(c).ok()
}

/// Ctrl+click: go to what the word names (class, or method declared in this file / its class).
fn definition(st: &State, top: &str, text: &str, line: usize, col: usize) -> Option<Act> {
    let (word, call, is_new, recv) = word_at(text, line, col)?;
    if call && !is_new {
        let in_file = recv.as_deref().is_none_or(|r| r == "this" || r == "super");
        if in_file {
            return find_decl(text, &word, None).map(Act::Line);
        }
        let cls = recv
            .as_deref()
            .and_then(|r| resolve_class(st, text, top, r))?;
        return Some(Act::OpenClass(
            cls,
            Some(Target {
                decl: Some((word, None)),
                then: None,
            }),
        ));
    }
    if word.starts_with(|c: char| c.is_uppercase()) {
        if let Some(c) = resolve_class(st, text, top, &word) {
            return Some(Act::OpenClass(c, None));
        }
        // a nested class of this file
        return text
            .lines()
            .position(|l| java_res()[0].captures(l).is_some_and(|c| c[2] == word))
            .map(|k| Act::Line(k + 1));
    }
    None
}

/// Right-click menu for `doc.menu_at`; returns the chosen action.
fn line_menu(ui: &mut egui::Ui, doc: &Doc, text: &str, st: Option<&State>) -> Option<Act> {
    let (at, col) = doc.menu_at?;
    let mut out = None;
    let mut pick = |ui: &mut egui::Ui, label: &str, a: Act| {
        if ui.button(label).clicked() {
            out = Some(a);
        }
    };
    let st = st?;
    if let Open::Report(_) = &doc.open {
        let link = doc
            .links
            .as_ref()
            .and_then(|l| l.get(at).cloned().flatten());
        match link {
            Some(l) => {
                if l.then.is_some() {
                    pick(
                        ui,
                        "Go to the call",
                        Act::GoTo(Link {
                            method: None,
                            ..l.clone()
                        }),
                    );
                }
                if let Some(m) = &l.method {
                    pick(
                        ui,
                        "Open this method",
                        Act::GoTo(Link {
                            at: m.clone(),
                            then: None,
                            method: None,
                        }),
                    );
                    if let Some((c, name)) = m.split_once("->").map(|(c, r)| {
                        (c.to_string(), r.split('(').next().unwrap_or("").to_string())
                    }) {
                        pick(
                            ui,
                            "Its callers",
                            Act::Callers(c.clone(), Some(name.clone())),
                        );
                        pick(ui, "Its call graph", Act::Graph(c, Some(name)));
                    }
                }
            }
            None => {
                ui.label("Click a line with a method to jump there");
            }
        }
        return out;
    }
    let Open::Class(top) = &doc.open else {
        ui.label("Nothing to do here");
        return None;
    };
    if doc.view == View::Smali {
        ui.label("Switch to Java for code actions");
        return None;
    }
    let line = text.lines().nth(at).unwrap_or("");
    // the word under the cursor: a class, a call or a name
    let decl = member_at(text, at);
    if let Some((word, call, is_new, recv)) = word_at(text, at, col) {
        let is_decl_name = decl
            .as_ref()
            .is_some_and(|(c, m)| m.as_ref().map_or(c.last() == Some(&word), |m| m.1 == word));
        if !is_decl_name {
            let class = if word.starts_with(|c: char| c.is_uppercase()) && !call || is_new {
                resolve_class(st, text, top, &word)
            } else {
                None
            };
            if let Some(cls) = &class {
                ui.label(RichText::new(format!("class {word}")).strong());
                pick(
                    ui,
                    "Open class   (Ctrl+click)",
                    Act::OpenClass(cls.clone(), None),
                );
                if let Some(d) = desc_of(st, cls) {
                    pick(
                        ui,
                        &format!("Where {word} is created (new {word})"),
                        Act::Callers(d.clone(), Some("<init>".into())),
                    );
                    pick(
                        ui,
                        &format!("Callers of any {word} method"),
                        Act::Callers(d, None),
                    );
                }
                pick(ui, "Find usages", Act::Usages(word.clone()));
                ui.separator();
            } else if call {
                ui.label(RichText::new(format!("{word}()")).strong());
                let owner = match recv.as_deref() {
                    None | Some("this" | "super") => owner_desc(st, top, &classes_around(text, at)),
                    Some(r) => resolve_class(st, text, top, r).and_then(|c| desc_of(st, &c)),
                };
                let real = owner.as_ref().and_then(|o| {
                    crate::re::real_member(&st.dexes, o, &word, true).map(|n| (o.clone(), n))
                });
                if let Some(a) = definition(st, top, text, at, col) {
                    pick(ui, "Go to definition   (Ctrl+click)", a);
                }
                if let Some((o, n)) = real {
                    pick(ui, "Callers", Act::Callers(o.clone(), Some(n.clone())));
                    pick(ui, "Call graph", Act::Graph(o.clone(), Some(n.clone())));
                    pick(
                        ui,
                        "Hook live (Frida)",
                        Act::LiveHook(o.clone(), Some(n.clone())),
                    );
                    pick(ui, "Frida hook (copy)", Act::Frida(o, Some(n)));
                }
                pick(ui, "Find usages", Act::Usages(word.clone()));
                ui.separator();
            } else if !KEYWORDS.contains(&word.as_str()) {
                ui.label(RichText::new(word.as_str()).strong());
                let owner = owner_desc(st, top, &classes_around(text, at));
                if let Some(o) =
                    owner.filter(|o| crate::re::real_member(&st.dexes, o, &word, false).is_some())
                {
                    let n = crate::re::real_member(&st.dexes, &o, &word, false).unwrap_or_default();
                    pick(ui, "Reads & writes of this field", Act::Access(o, n));
                }
                pick(ui, "Find usages in the app", Act::Usages(word.clone()));
                ui.label(RichText::new("Click a name to mark it everywhere in this file").small());
                ui.separator();
            }
        }
    }
    let Some((chain, member)) = decl else {
        if out.is_none() && word_at(text, at, col).is_none() {
            ui.label("Right-click a name, or a class, method or field declaration");
        }
        return out;
    };
    let owner = owner_desc(st, top, &chain)?;
    let real = member.as_ref().and_then(|(m, n)| {
        crate::re::real_member(&st.dexes, &owner, n, *m).map(|r| (*m, r, n.clone()))
    });
    match &real {
        Some((true, name, shown)) => {
            ui.label(RichText::new(format!("method {shown}()")).strong());
            pick(
                ui,
                "Callers",
                Act::Callers(owner.clone(), Some(name.clone())),
            );
            pick(
                ui,
                "Call graph",
                Act::Graph(owner.clone(), Some(name.clone())),
            );
            pick(
                ui,
                if line.contains("WARNING") {
                    "Show this method in Smali"
                } else {
                    "Show Smali"
                },
                Act::Smali(top.clone(), name.clone()),
            );
            pick(
                ui,
                "Hook live (Frida)",
                Act::LiveHook(owner.clone(), Some(name.clone())),
            );
            pick(
                ui,
                "Frida hook (copy)",
                Act::Frida(owner.clone(), Some(name.clone())),
            );
            ui.separator();
            pick(
                ui,
                "Rename…",
                Act::Note(
                    format!("{owner}->{name}()"),
                    false,
                    format!("Rename method {shown}"),
                ),
            );
            pick(
                ui,
                "Comment…",
                Act::Note(
                    format!("{owner}->{name}()"),
                    true,
                    format!("Comment on {shown}()"),
                ),
            );
        }
        Some((false, name, shown)) => {
            ui.label(RichText::new(format!("field {shown}")).strong());
            pick(
                ui,
                "Reads & writes",
                Act::Access(owner.clone(), name.clone()),
            );
            ui.separator();
            pick(
                ui,
                "Rename…",
                Act::Note(
                    format!("{owner}->{name}"),
                    false,
                    format!("Rename field {shown}"),
                ),
            );
            pick(
                ui,
                "Comment…",
                Act::Note(
                    format!("{owner}->{name}"),
                    true,
                    format!("Comment on {shown}"),
                ),
            );
        }
        None if member.is_some() => {
            ui.label("Not found in the DEX (synthetic or inlined code)");
        }
        None => {
            let shown = chain.last().cloned().unwrap_or_default();
            ui.label(RichText::new(format!("class {shown}")).strong());
            pick(
                ui,
                &format!("Where {shown} is created"),
                Act::Callers(owner.clone(), Some("<init>".into())),
            );
            pick(
                ui,
                "Callers (any method)",
                Act::Callers(owner.clone(), None),
            );
            pick(
                ui,
                "Hook all methods live (Frida)",
                Act::LiveHook(owner.clone(), None),
            );
            pick(
                ui,
                "Frida hooks (all methods, copy)",
                Act::Frida(owner.clone(), None),
            );
            ui.separator();
            pick(
                ui,
                "Rename…",
                Act::Note(
                    owner.clone(),
                    false,
                    format!("Rename class {shown} (a short name keeps the package)"),
                ),
            );
            pick(
                ui,
                "Comment…",
                Act::Note(owner.clone(), true, format!("Comment on class {shown}")),
            );
        }
    }
    out
}

enum Msg {
    Loaded(Result<State, String>),
    Text(Open, View, String),
    Report(Open, Vec<Line>),
    Hits(u64, Vec<Hit>),
    /// (query, matching strings)
    Strings(String, Vec<(String, usize)>),
    Saved(Result<String, String>),
    /// a background job panicked: (what it was doing, why)
    Failed(&'static str, String),
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Classes,
    Resources,
    Strings,
    Native,
    Files,
}

/// "Edit strings" window
struct StringsDlg {
    /// the resources.arsc the edits apply to (with any pending changes)
    base: Vec<u8>,
    all: Vec<arsc::StringRes>,
    config: String,
    filter: String,
    /// (config, name) -> new text
    changes: std::collections::BTreeMap<(String, String), String>,
}

/// "Build & sign" dialog
struct BuildDlg {
    /// sign with the user's own key instead of apkscope's
    own_key: bool,
    key: String,
    cert: String,
}

struct ExportDlg {
    fmt: Format,
    ctx: usize,
    method: bool,
}

pub struct App {
    path: String,
    set: Settings,
    st: Option<&'static State>,
    err: Option<String>,
    /// class trees for App, Other and Library code
    trees: [Tree; 3],
    counts: [usize; 3],
    show_libs: bool,
    app_only: bool,
    res: BTreeMap<String, Vec<String>>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    docs: Vec<Doc>,
    active: usize,
    side: Side,
    filter: String,
    strings_q: String,
    /// query the shown strings were listed for
    strings_for: Option<String>,
    strings: Vec<(String, usize)>,
    natives: Option<Vec<(String, u64)>>,
    files: Option<Vec<(String, u64, u64)>>,
    /// pending changes to the APK: name -> new content, or None to delete
    edits: rebuild::Edits,
    /// edited Smali class sources, assembled into DEX at build time
    smali_edits: Vec<String>,
    /// "Add file" dialog: (path inside the APK, content)
    adding: Option<(String, Vec<u8>)>,
    build: Option<BuildDlg>,
    strings_dlg: Option<StringsDlg>,
    query: String,
    scope: Scope,
    use_regex: bool,
    case: bool,
    hits: Vec<Hit>,
    hits_title: String,
    hits_secs: f32,
    search_started: Option<std::time::Instant>,
    search_err: Option<String>,
    search_id: u64,
    prog: Option<Arc<Progress>>,
    focus_search: bool,
    show_settings: bool,
    export: Option<ExportDlg>,
    toast: Option<(String, std::time::Instant)>,
    cache_size: Option<u64>,
    styled: Option<(Theme, u32)>,
    note_dlg: Option<NoteDlg>,
    /// navigation history: (tab, view, marked line)
    back: Vec<(Open, View, Option<usize>)>,
    fwd: Vec<(Open, View, Option<usize>)>,
    /// search results: classes folded away, and a filter over the hits
    folded: std::collections::HashSet<String>,
    hits_filter: String,
    titled: bool,
    logo: Option<egui::TextureHandle>,
    live: live_ui::LiveUi,
}

/// One-line text box whose hint is also its name for screen readers.
fn edit(ui: &mut egui::Ui, buf: &mut String, hint: &str, width: f32) -> egui::Response {
    let r = ui.add(
        egui::TextEdit::singleline(buf)
            .hint_text(hint)
            .desired_width(width),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, hint));
    r
}

/// Run `f` in the background. If it panics, the UI hears about it instead of waiting forever.
fn spawn(tx: &mpsc::Sender<Msg>, what: &'static str, f: impl FnOnce() + Send + 'static) {
    let mine = tx.clone();
    // decompiling recurses per nesting level
    let ok = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let tx = mine;
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                let why = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown error".into());
                let _ = tx.send(Msg::Failed(what, why));
            }
        });
    if let Err(e) = ok {
        let _ = tx.send(Msg::Failed(what, e.to_string()));
    }
}

impl App {
    fn new(path: Option<String>, deobf_flag: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut set = Settings::load();
        // --no-deobf on the command line wins over the setting
        set.deobf &= deobf_flag;
        session::THREADS.store(set.threads, Ordering::Relaxed);
        let mut app = App {
            path: String::new(),
            set,
            st: None,
            err: None,
            trees: Default::default(),
            counts: [0; 3],
            show_libs: false,
            app_only: false,
            res: BTreeMap::new(),
            tx,
            rx,
            docs: vec![],
            active: 0,
            side: Side::Classes,
            filter: String::new(),
            strings_q: String::new(),
            strings_for: None,
            strings: vec![],
            natives: None,
            files: None,
            edits: rebuild::Edits::new(),
            smali_edits: vec![],
            adding: None,
            build: None,
            strings_dlg: None,
            query: String::new(),
            scope: Scope::Code,
            use_regex: false,
            case: false,
            hits: vec![],
            hits_title: String::new(),
            hits_secs: 0.0,
            search_started: None,
            search_err: None,
            search_id: 0,
            prog: None,
            focus_search: false,
            show_settings: false,
            export: None,
            toast: None,
            cache_size: None,
            styled: None,
            note_dlg: None,
            back: vec![],
            fwd: vec![],
            folded: Default::default(),
            hits_filter: String::new(),
            titled: false,
            logo: None,
            live: Default::default(),
        };
        if let Some(p) = path {
            app.start_load(p);
        }
        app
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), std::time::Instant::now()));
    }

    fn start_load(&mut self, path: String) {
        // the name tables are process-wide, so a second APK gets its own window
        if !self.path.is_empty() {
            if let Ok(exe) = std::env::current_exe() {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("gui").arg(&path);
                if !self.set.deobf {
                    cmd.arg("--no-deobf");
                }
                let _ = cmd.spawn();
            }
            return;
        }
        self.path = path.clone();
        let (tx, deobf) = (self.tx.clone(), self.set.deobf);
        spawn(&self.tx, "load", move || {
            let _ = tx.send(Msg::Loaded(session::load(&path, deobf)));
        });
    }

    /// Show `what` in its tab (opening one if needed), optionally scrolled to a line.
    fn open(&mut self, what: Open, view: Option<View>, mark: Option<usize>) {
        let Some(st) = self.st else { return };
        let k = match self.docs.iter().position(|d| d.open == what) {
            Some(k) => k,
            None => {
                self.docs.push(Doc::new(what.clone()));
                self.docs.len() - 1
            }
        };
        self.active = k;
        let d = &mut self.docs[k];
        if mark.is_some() {
            d.mark = mark;
            d.scroll_pending = true;
            d.job = None;
        }
        let view = view.unwrap_or(d.view);
        if d.text.is_some() && d.view == view {
            return;
        }
        d.view = view;
        d.text = None;
        d.job = None;
        let tx = self.tx.clone();
        spawn(&self.tx, "open", move || {
            let text = match &what {
                Open::Overview => Some(session::overview(st)),
                Open::Class(c) => st.class_text(c, view == View::Smali),
                Open::Res(p) => st.resource(p),
                Open::Native(p) => Some(st.native_report(p)),
                Open::File(p) => Some(st.file_view(p)),
                Open::Report(_) => None,
            };
            let _ = tx.send(Msg::Text(
                what,
                view,
                text.unwrap_or_else(|| "// not found".into()),
            ));
        });
    }

    /// Open a generated report in its own tab, computed off the UI thread. Lines with a link
    /// jump to the code when clicked.
    fn report(
        &mut self,
        title: String,
        f: impl FnOnce(&'static State) -> Vec<Line> + Send + 'static,
    ) {
        let Some(st) = self.st else { return };
        self.push_nav();
        let open = Open::Report(title);
        self.docs.retain(|d| d.open != open);
        self.docs.push(Doc::new(open.clone()));
        self.active = self.docs.len() - 1;
        let tx = self.tx.clone();
        spawn(&self.tx, "report", move || {
            let _ = tx.send(Msg::Report(open, f(st)));
        });
    }

    /// Remember where we are before jumping somewhere else (Back = Alt+Left).
    fn push_nav(&mut self) {
        if let Some(d) = self.docs.get(self.active) {
            self.back.push((d.open.clone(), d.view, d.mark));
            self.fwd.clear();
            if self.back.len() > 100 {
                self.back.remove(0);
            }
        }
    }

    fn nav(&mut self, forward: bool) {
        let (from, to) = if forward {
            (&mut self.fwd, &mut self.back)
        } else {
            (&mut self.back, &mut self.fwd)
        };
        let Some((o, v, m)) = from.pop() else { return };
        if let Some(d) = self.docs.get(self.active) {
            to.push((d.open.clone(), d.view, d.mark));
        }
        if matches!(o, Open::Report(_)) && !self.docs.iter().any(|d| d.open == o) {
            return; // closed report: nothing to go back to
        }
        self.open(o, Some(v), m);
    }

    /// Open a link's method, at the call or field use it points to.
    fn go_to(&mut self, l: &Link) {
        let Some(st) = self.st else { return };
        let Some((desc, rest)) = l.at.split_once("->") else {
            return;
        };
        let top = desc
            .find('$')
            .map_or(desc.to_string(), |k| format!("{};", &desc[..k]));
        let Some(n) = st
            .classes
            .iter()
            .find(|x| st.dexes[x.1].class_descriptor(x.2).ok().as_deref() == Some(top.as_str()))
            .map(|x| x.0.clone())
        else {
            self.toast(format!(
                "{} is not in this APK",
                crate::dex::java_name(desc)
            ));
            return;
        };
        let name = rest.split('(').next().unwrap_or("");
        let shown = match name {
            "<init>" => crate::dex::java_name(desc)
                .rsplit(['.', '$'])
                .next()
                .unwrap_or("")
                .to_string(),
            n => crate::deobf::method(desc, n).unwrap_or_else(|| n.to_string()),
        };
        let nargs = rest
            .split_once('(')
            .map(|x| crate::re::split_params(x.1.split(')').next().unwrap_or("")).len());
        let decl = (name != "<clinit>").then_some((shown, nargs));
        self.push_nav();
        self.open(Open::Class(n), Some(View::Java), None);
        self.find_in_active(Target {
            decl,
            then: l.then.clone(),
        });
    }

    /// Jump to a target in the active tab now, or once its text arrives.
    fn find_in_active(&mut self, t: Target) {
        let Some(d) = self.docs.get_mut(self.active) else {
            return;
        };
        match &d.text {
            Some(text) => {
                if let Some(l) = locate(text, &t) {
                    d.mark = Some(l);
                    d.scroll_pending = true;
                    d.job = None;
                }
            }
            None => d.find_on_load = Some(t),
        }
    }

    /// Store a rename or comment in the project file and redraw the open classes with it.
    fn save_note(&mut self, key: &str, comment: bool, text: &str) {
        let Some(st) = self.st else { return };
        let mut n = crate::deobf::notes();
        crate::re::apply_note(&mut n, key, comment, text);
        if let Err(e) = crate::re::save_notes(&st.path, &n) {
            self.toast(format!("Not saved: {e}"));
            return;
        }
        crate::deobf::set_notes(n);
        // cached code has the old names; and new code must not go under the old cache key
        st.drop_cache();
        session::CACHE_WRITE.store(false, Ordering::Relaxed);
        let active = self.active;
        let reopen: Vec<(Open, View)> = self
            .docs
            .iter_mut()
            .filter(|d| matches!(d.open, Open::Class(_)))
            .map(|d| {
                d.text = None;
                (d.open.clone(), d.view)
            })
            .collect();
        for (o, v) in reopen {
            self.open(o, Some(v), None);
        }
        self.active = active.min(self.docs.len().saturating_sub(1));
        self.toast(format!(
            "Saved to {}",
            crate::re::project_file(&st.path).display()
        ));
    }

    /// Carry out a context-menu choice.
    fn act(&mut self, a: Act, ctx: &egui::Context) {
        let Some(st) = self.st else { return };
        let short = |c: &str, m: &Option<String>| {
            format!(
                "{}{}",
                crate::dex::java_name(c).rsplit('.').next().unwrap_or(""),
                m.as_ref().map(|m| format!(".{m}")).unwrap_or_default()
            )
        };
        match a {
            Act::Callers(c, m) => {
                let title = if m.as_deref() == Some("<init>") {
                    format!("new {}", short(&c, &None))
                } else {
                    format!("Callers {}", short(&c, &m))
                };
                self.report(title, move |st| {
                    crate::re::callers_report(&st.dexes, &c, m.as_deref())
                })
            }
            Act::Graph(c, m) => self.report(format!("Graph {}", short(&c, &m)), move |st| {
                crate::re::graph_report(&st.dexes, &c, m.as_deref(), 3)
            }),
            Act::Access(c, f) => self.report(
                format!("Access {}", short(&c, &Some(f.clone()))),
                move |st| crate::re::access_report(&st.dexes, &c, &f),
            ),
            Act::Frida(c, m) => match crate::re::frida(&st.dexes, &c, m.as_deref(), false) {
                Ok(js) => {
                    ctx.copy_text(js.clone());
                    self.report(format!("Frida {}", short(&c, &m)), move |_| {
                        js.lines().map(|l| (l.to_string(), None)).collect()
                    });
                    self.toast("Frida script copied to the clipboard");
                }
                Err(e) => self.toast(e),
            },
            Act::LiveHook(c, m) => {
                let r = self.live.hook(st, &c, m.as_deref());
                self.toast(r.unwrap_or_else(|e| e));
            }
            Act::Smali(class_name, method) => {
                self.push_nav();
                self.open(Open::Class(class_name), Some(View::Smali), None);
                self.find_in_active(Target {
                    decl: None,
                    then: Some(format!(" {method}(")),
                });
            }
            Act::OpenClass(name, target) => {
                self.push_nav();
                self.open(Open::Class(name), Some(View::Java), None);
                if let Some(t) = target {
                    self.find_in_active(t);
                }
            }
            Act::Line(l) => {
                self.push_nav();
                if let Some(d) = self.docs.get_mut(self.active) {
                    d.mark = Some(l);
                    d.scroll_pending = true;
                    d.job = None;
                }
            }
            Act::Usages(w) => self.usages_of(&w),
            Act::Note(key, comment, label) => {
                let n = crate::deobf::notes();
                let class = !key.contains("->");
                let cur = if comment {
                    n.comments.get(&key)
                } else if class {
                    n.classes.get(&key)
                } else {
                    n.members.get(&key)
                };
                let text = cur
                    .map(|v| {
                        if !comment && class {
                            crate::dex::java_name(v)
                        } else {
                            v.clone()
                        }
                    })
                    .unwrap_or_default();
                self.note_dlg = Some(NoteDlg {
                    key,
                    comment,
                    label,
                    text,
                });
            }
            Act::GoTo(l) => self.go_to(&l),
        }
    }

    fn note_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.note_dlg else {
            return;
        };
        let mut done = None;
        let mut open = true;
        egui::Window::new(if dlg.comment { "Comment" } else { "Rename" }).collapsible(false).open(&mut open).show(ctx, |ui| {
            ui.label(&dlg.label);
            let r = if dlg.comment {
                ui.add(egui::TextEdit::multiline(&mut dlg.text).desired_rows(4).desired_width(420.0))
            } else {
                edit(ui, &mut dlg.text, "new name", 320.0)
            };
            ui.label(RichText::new("Saved next to the APK in <apk>.apkscope.json. `apkscope map` exports it for JADX / Enigma.").small());
            ui.horizontal(|ui| {
                let enter = !dlg.comment && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Save").clicked() || enter {
                    done = Some((dlg.key.clone(), dlg.comment, dlg.text.trim().to_string()));
                }
                if ui.button("Remove").on_hover_text("Back to the original name / no comment").clicked() {
                    done = Some((dlg.key.clone(), dlg.comment, String::new()));
                }
            });
        });
        if let Some((k, c, t)) = done {
            self.note_dlg = None;
            self.save_note(&k, c, &t);
        } else if !open {
            self.note_dlg = None;
        }
    }

    fn close(&mut self, k: usize) {
        self.docs.remove(k);
        if self.active >= self.docs.len() {
            self.active = self.docs.len().saturating_sub(1);
        }
    }

    fn run_search(&mut self) {
        let Some(st) = self.st else { return };
        if self.query.is_empty() && !self.scope.query_optional() {
            return;
        }
        let m = match Matcher::new(&self.query, self.scope, self.use_regex, self.case) {
            Ok(mut m) => {
                m.app_only = self.app_only;
                m
            }
            Err(e) => {
                self.search_err = Some(e);
                return;
            }
        };
        self.cancel_search();
        self.search_id += 1;
        self.search_err = None;
        self.folded.clear();
        self.hits_filter.clear();
        self.hits.clear();
        self.hits_title = if self.query.is_empty() {
            self.scope.label().to_string()
        } else {
            format!("{}: \"{}\"", self.scope.label(), self.query)
        };
        if self.app_only {
            self.hits_title += " (app code only)";
        }
        self.search_started = Some(std::time::Instant::now());
        let prog = Arc::new(Progress::default());
        self.prog = Some(prog.clone());
        let (tx, id) = (self.tx.clone(), self.search_id);
        spawn(&self.tx, "search", move || {
            let _ = tx.send(Msg::Hits(id, session::search(st, &m, &prog)));
        });
    }

    /// Stop the running search and give the UI back now; a class stuck decompiling finishes
    /// on its own and its late result is dropped (stale search id).
    fn cancel_search(&mut self) {
        if let Some(p) = self.prog.take() {
            p.cancel.store(true, Ordering::Relaxed);
            self.search_id += 1;
            self.hits_secs = self
                .search_started
                .map_or(0.0, |t| t.elapsed().as_secs_f32());
            self.hits_title += " (cancelled)";
        }
    }

    /// Usages of an exact name or string, as from the Strings tab.
    fn usages_of(&mut self, s: &str) {
        self.scope = Scope::Usages;
        self.query = s.to_string();
        self.use_regex = false;
        self.case = true;
        self.run_search();
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Loaded(Ok(st)) => {
                    let st: &'static State = Box::leak(Box::new(st));
                    for (k, o) in [Origin::App, Origin::Other, Origin::Library]
                        .into_iter()
                        .enumerate()
                    {
                        let mine: Vec<&String> = st
                            .classes
                            .iter()
                            .zip(&st.origin)
                            .filter(|x| *x.1 == o)
                            .map(|x| &x.0.0)
                            .collect();
                        self.counts[k] = mine.len();
                        self.trees[k] = tree(mine.into_iter());
                    }
                    self.res = res_groups(st);
                    self.strings_for = None;
                    self.strings.clear();
                    self.st = Some(st);
                    spawn(&self.tx, "index", move || st.build_index());
                    self.open(Open::Overview, None, None);
                }
                Msg::Loaded(Err(e)) => {
                    // nothing is loaded, so the next file opens here instead of in a new window
                    self.err = Some(e);
                    self.path.clear();
                }
                Msg::Text(open, view, text) => {
                    if let Some(d) = self
                        .docs
                        .iter_mut()
                        .find(|d| d.open == open && d.view == view)
                    {
                        if let Some(l) = d.find_on_load.take().and_then(|t| locate(&text, &t)) {
                            d.mark = Some(l);
                            d.scroll_pending = true;
                        }
                        d.text = Some(Arc::new(text));
                        d.job = None;
                    }
                }
                Msg::Report(open, lines) => {
                    if let Some(d) = self.docs.iter_mut().find(|d| d.open == open) {
                        let text: String = lines.iter().map(|l| format!("{}\n", l.0)).collect();
                        d.links = Some(Arc::new(lines.into_iter().map(|l| l.1).collect()));
                        d.text = Some(Arc::new(text));
                        d.job = None;
                    }
                }
                Msg::Hits(id, h) if id == self.search_id => {
                    self.prog = None;
                    self.hits = h;
                    // stable: lines stay in order within a class
                    self.hits.sort_by(|a, b| a.0.cmp(&b.0));
                    self.hits_secs = self
                        .search_started
                        .map_or(0.0, |t| t.elapsed().as_secs_f32());
                }
                Msg::Hits(..) => {}
                Msg::Strings(q, v) if self.strings_for.as_deref() == Some(q.as_str()) => {
                    self.strings = v
                }
                Msg::Strings(..) => {}
                Msg::Saved(Ok(s)) => self.toast(s),
                Msg::Saved(Err(e)) => self.toast(format!("Export failed: {e}")),
                Msg::Failed(what, why) => {
                    let msg = format!("Something went wrong ({what}): {why}");
                    match what {
                        "load" => {
                            self.err = Some(msg.clone());
                            self.path.clear();
                        }
                        "search" => {
                            self.prog = None;
                            self.search_err = Some(msg.clone());
                        }
                        // tabs still waiting for text would spin forever
                        "open" | "report" => {
                            for d in self.docs.iter_mut().filter(|d| d.text.is_none()) {
                                d.text = Some(Arc::new(format!("// {msg}\n")));
                                d.job = None;
                            }
                        }
                        _ => {}
                    }
                    self.toast(msg);
                }
            }
        }
    }

    fn style(&mut self, ctx: &egui::Context) {
        let key = (self.set.theme, (self.set.ui_scale * 100.0) as u32);
        if self.styled == Some(key) {
            return;
        }
        self.styled = Some(key);
        let p = self.set.theme.palette();
        let mut v = if self.set.theme.dark() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = p.panel;
        v.window_fill = p.panel;
        v.extreme_bg_color = p.bg;
        v.faint_bg_color = p.bg;
        v.override_text_color = Some(p.text);
        v.hyperlink_color = p.accent;
        v.selection.bg_fill = p.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0_f32, p.accent);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
            &mut v.widgets.noninteractive,
        ] {
            w.corner_radius = CornerRadius::same(6);
        }
        // a hairline so text fields stand out from the panel they sit on
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.dim.gamma_multiply(0.35));
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent.gamma_multiply(0.6));
        v.widgets.active.bg_fill = p.accent.gamma_multiply(0.6);
        ctx.set_visuals(v);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 5.0);
            s.spacing.button_padding = egui::vec2(10.0, 4.0);
            s.spacing.interact_size.y = 24.0;
        });
        ctx.set_zoom_factor(self.set.ui_scale);
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.set.theme.palette();
        ui.horizontal(|ui| {
            for (s, n) in [
                (Side::Classes, "Classes"),
                (Side::Resources, "Resources"),
                (Side::Strings, "Strings"),
                (Side::Native, "Native"),
                (Side::Files, "Files"),
            ] {
                let sel = self.side == s;
                let t = RichText::new(n)
                    .strong()
                    .color(if sel { p.accent } else { p.dim });
                if ui.add(egui::Button::new(t).frame(false)).clicked() {
                    self.side = s;
                }
            }
        });
        ui.add_space(2.0);
        let Some(st) = self.st else {
            ui.label(RichText::new("No APK open").color(p.dim));
            return;
        };
        if self.side == Side::Files {
            let files = self.files.get_or_insert_with(|| st.files_list());
            let cur = self.docs.get(self.active).map(|d| d.open.clone());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} files", files.len())).color(p.dim));
                if ui
                    .button("Extract all…")
                    .on_hover_text("Write every file of the APK to a folder")
                    .clicked()
                {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let tx = self.tx.clone();
                        spawn(&self.tx, "save", move || {
                            let r = st
                                .extract_all(&dir)
                                .map(|n| format!("Extracted {n} files to {}", dir.display()));
                            let _ = tx.send(Msg::Saved(r));
                        });
                    }
                }
            });
            let mut verify = false;
            ui.horizontal(|ui| {
                if ui
                    .button("Add file…")
                    .on_hover_text("Put a new file into the APK when you build")
                    .clicked()
                {
                    if let Some(f) = rfd::FileDialog::new().pick_file() {
                        match std::fs::read(&f) {
                            Ok(b) => {
                                let name = f
                                    .file_name()
                                    .map_or("file".into(), |n| n.to_string_lossy().into_owned());
                                self.adding = Some((format!("assets/{name}"), b));
                            }
                            Err(e) => {
                                self.toast = Some((
                                    format!("Can't read {}: {e}", f.display()),
                                    std::time::Instant::now(),
                                ))
                            }
                        }
                    }
                }
                let n = self.edits.len();
                let build = ui.add_enabled(
                    n > 0,
                    egui::Button::new(RichText::new(format!("Build & sign ({n})")).strong())
                        .fill(p.accent.gamma_multiply(0.55)),
                );
                if build
                    .on_hover_text(
                        "Write a new APK with your changes, zipaligned and signed (v2 + v3)",
                    )
                    .clicked()
                {
                    self.build = Some(BuildDlg {
                        own_key: false,
                        key: String::new(),
                        cert: String::new(),
                    });
                }
                verify = ui
                    .button("Verify signature")
                    .on_hover_text("Check this APK's v2/v3 signatures")
                    .clicked();
            });
            if verify {
                let (tx, file) = (self.tx.clone(), st.file.clone());
                spawn(&self.tx, "save", move || {
                    let r = apksign::verify(&file).map(|l| l.join(", "));
                    let _ = tx.send(Msg::Saved(
                        r.map(|l| format!("Signature: {l}")).map_err(String::from),
                    ));
                });
            }
            if !self.edits.is_empty() {
                let mut undo = None;
                egui::CollapsingHeader::new(
                    RichText::new(format!("Changes ({})", self.edits.len()))
                        .strong()
                        .color(p.accent),
                )
                .id_salt("changes")
                .default_open(true)
                .show(ui, |ui| {
                    for (name, new) in &self.edits {
                        ui.horizontal(|ui| {
                            let what = match new {
                                Some(b) if files.iter().any(|f| &f.0 == name) => {
                                    format!("replace  {name}  ({})", mb(b.len() as u64))
                                }
                                Some(b) => format!("add  {name}  ({})", mb(b.len() as u64)),
                                None => format!("delete  {name}"),
                            };
                            if ui.small_button("undo").clicked() {
                                undo = Some(name.clone());
                            }
                            ui.label(what);
                        });
                    }
                });
                if let Some(n) = undo {
                    self.edits.remove(&n);
                }
            }
            edit(ui, &mut self.filter, "Filter files…", f32::INFINITY);
            ui.separator();
            let filter = self.filter.to_lowercase();
            let mut go = None;
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    let mut dirs: BTreeMap<&str, Vec<&(String, u64, u64)>> = BTreeMap::new();
                    for f in files
                        .iter()
                        .filter(|f| filter.is_empty() || f.0.to_lowercase().contains(&filter))
                    {
                        dirs.entry(f.0.rsplit_once('/').map_or("", |x| x.0))
                            .or_default()
                            .push(f);
                    }
                    for (dir, list) in dirs {
                        let mut row = |ui: &mut egui::Ui, f: &(String, u64, u64)| {
                            let name = f.0.rsplit('/').next().unwrap_or(&f.0);
                            let open = if f.0.ends_with(".so") {
                                Open::Native(f.0.clone())
                            } else {
                                Open::File(f.0.clone())
                            };
                            // pending changes show in the list too
                            let mark = match self.edits.get(&f.0) {
                                Some(Some(_)) => "  (replaced)",
                                Some(None) => "  (deleted)",
                                None => "",
                            };
                            if ui
                                .selectable_label(
                                    cur.as_ref() == Some(&open),
                                    format!("{name}   {}{mark}", mb(f.1)),
                                )
                                .on_hover_text(format!(
                                    "{}\n{} bytes, {} compressed",
                                    f.0, f.1, f.2
                                ))
                                .clicked()
                            {
                                go = Some(open);
                            }
                        };
                        if dir.is_empty() {
                            list.into_iter().for_each(|f| row(ui, f));
                        } else {
                            egui::CollapsingHeader::new(format!("{dir}  ({})", list.len()))
                                .id_salt(("files", dir))
                                .default_open(!filter.is_empty())
                                .show(ui, |ui| list.into_iter().for_each(|f| row(ui, f)));
                        }
                    }
                });
            if let Some(o) = go {
                self.open(o, None, None);
            }
            return;
        }
        if self.side == Side::Native {
            let libs = self.natives.get_or_insert_with(|| st.native_libs());
            let cur = self.docs.get(self.active).map(|d| d.open.clone());
            ui.label(RichText::new("Native libraries (.so) per CPU architecture. Open one for its JNI methods, exports, dependencies and strings.").small().color(p.dim));
            ui.separator();
            if libs.is_empty() {
                ui.label(RichText::new("This APK has no native libraries.").color(p.dim));
                return;
            }
            let mut go = None;
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    let mut abis: Vec<&str> = libs
                        .iter()
                        .map(|l| l.0.split('/').nth(1).unwrap_or(""))
                        .collect();
                    abis.dedup();
                    for abi in abis {
                        let mine: Vec<&(String, u64)> = libs
                            .iter()
                            .filter(|l| l.0.split('/').nth(1) == Some(abi))
                            .collect();
                        let total: u64 = mine.iter().map(|l| l.1).sum();
                        egui::CollapsingHeader::new(
                            RichText::new(format!("{abi}  ({}, {})", mine.len(), mb(total)))
                                .strong(),
                        )
                        .id_salt(("abi", abi))
                        .default_open(abi == "arm64-v8a")
                        .show(ui, |ui| {
                            for (path, size) in mine {
                                let name = path.rsplit('/').next().unwrap_or(path);
                                if ui
                                    .selectable_label(
                                        cur.as_ref() == Some(&Open::Native(path.clone())),
                                        format!("{name}   {}", mb(*size)),
                                    )
                                    .clicked()
                                {
                                    go = Some(path.clone());
                                }
                            }
                        });
                    }
                });
            if let Some(path) = go {
                self.open(Open::Native(path), None, None);
            }
            return;
        }
        if self.side == Side::Strings {
            edit(ui, &mut self.strings_q, "Filter strings…", f32::INFINITY);
            // list everything at first, then refilter as the user types (stale replies are dropped)
            // the index adds usage counts: list again once it's ready
            let key = format!("{} {}", self.strings_q, st.index.get().is_some());
            if self.strings_for.as_deref() != Some(key.as_str()) {
                self.strings_for = Some(key.clone());
                let (tx, q) = (self.tx.clone(), self.strings_q.clone());
                spawn(&self.tx, "strings", move || {
                    let v = st.strings(&q, session::LIMIT);
                    let _ = tx.send(Msg::Strings(key, v));
                });
            }
            let more = if self.strings.len() >= session::LIMIT {
                format!("first {}", session::LIMIT)
            } else {
                self.strings.len().to_string()
            };
            ui.label(
                RichText::new(format!(
                    "{more} strings{}. Click one to find where it's used.",
                    if st.index.get().is_some() {
                        " (number = classes using it)"
                    } else {
                        ""
                    }
                ))
                .small()
                .color(p.dim),
            );
            ui.separator();
            let mut go = None;
            let row = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            egui::ScrollArea::vertical().auto_shrink(false).show_rows(
                ui,
                row,
                self.strings.len(),
                |ui, rows| {
                    for (s, n) in &self.strings[rows] {
                        let shown: String = s.chars().take(120).collect();
                        let label = if st.index.get().is_some() {
                            format!("{n:>4}   {shown}")
                        } else {
                            shown
                        };
                        let r = ui
                            .add(
                                egui::Label::new(label)
                                    .truncate()
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if r.on_hover_text(s).clicked() {
                            go = Some(s.clone());
                        }
                    }
                },
            );
            if let Some(s) = go {
                self.usages_of(&s);
            }
            return;
        }
        let hint = if self.side == Side::Classes {
            "Filter classes…"
        } else {
            "Filter resources…"
        };
        edit(ui, &mut self.filter, hint, f32::INFINITY);
        if self.side == Side::Resources
            && ui
                .button("Edit strings…")
                .on_hover_text("Change string resources (app name, texts, URLs in strings.xml)")
                .clicked()
        {
            let base = match self.edits.get("resources.arsc") {
                Some(Some(b)) => Some(b.clone()),
                _ => st.read_entry("resources.arsc"),
            };
            match base.map(|b| arsc::strings(&b).map(|all| (b, all))) {
                Some(Ok((base, all))) => {
                    self.strings_dlg = Some(StringsDlg {
                        base,
                        all,
                        config: String::new(),
                        filter: String::new(),
                        changes: Default::default(),
                    })
                }
                Some(Err(e)) => {
                    self.toast = Some((
                        format!("Can't edit strings: {e}"),
                        std::time::Instant::now(),
                    ))
                }
                None => {
                    self.toast = Some((
                        "This APK has no resources.arsc".into(),
                        std::time::Instant::now(),
                    ))
                }
            }
        }
        if self.side == Side::Classes {
            ui.checkbox(&mut self.show_libs, format!("Show libraries ({})", self.counts[2])).on_hover_text("androidx, Kotlin, Google Play services, OkHttp, ad SDKs... They stay searchable either way.");
        }
        ui.separator();
        let cur = self.docs.get(self.active).map(|d| d.open.clone());
        let filter = self.filter.to_lowercase();
        let mut go = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| match self.side {
                Side::Classes if filter.len() >= 2 => {
                    let shown = st.classes.iter().zip(&st.origin).filter(|(c, o)| {
                        (self.show_libs || **o != Origin::Library)
                            && c.0.to_lowercase().contains(&filter)
                    });
                    for ((n, _, _), _) in shown.take(session::LIMIT) {
                        if ui
                            .selectable_label(cur.as_ref() == Some(&Open::Class(n.clone())), n)
                            .clicked()
                        {
                            go = Some(Open::Class(n.clone()));
                        }
                    }
                }
                Side::Classes => {
                    if ui
                        .selectable_label(
                            cur.as_ref() == Some(&Open::Overview),
                            RichText::new("Overview").strong(),
                        )
                        .clicked()
                    {
                        go = Some(Open::Overview);
                    }
                    let mut clicked = None;
                    let names = [
                        format!("App code · {}", st.package),
                        "Other code (unknown or obfuscated packages)".to_string(),
                        "Libraries & SDKs".to_string(),
                    ];
                    for k in 0..3 {
                        if self.counts[k] == 0 || (k == 2 && !self.show_libs) {
                            continue;
                        }
                        let head = RichText::new(format!("{}  ({})", names[k], self.counts[k]))
                            .strong()
                            .color(if k == 0 { p.accent } else { p.text });
                        egui::CollapsingHeader::new(head)
                            .id_salt(("origin", k))
                            .default_open(k < 2)
                            .show(ui, |ui| node(ui, &self.trees[k], "", &cur, &mut clicked));
                    }
                    if self.counts[2] > 0 && !self.show_libs {
                        ui.label(
                            RichText::new(format!("{} library classes hidden", self.counts[2]))
                                .small()
                                .color(p.dim),
                        );
                    }
                    if clicked.is_some() {
                        go = clicked.map(Open::Class);
                    }
                }
                _ => {
                    for (dir, paths) in &self.res {
                        let shown: Vec<&String> = paths
                            .iter()
                            .filter(|p| filter.is_empty() || p.to_lowercase().contains(&filter))
                            .collect();
                        if shown.is_empty() {
                            continue;
                        }
                        let mut leaf = |ui: &mut egui::Ui, p: &String| {
                            let name = p.rsplit('/').next().unwrap_or(p);
                            if ui
                                .selectable_label(cur.as_ref() == Some(&Open::Res(p.clone())), name)
                                .clicked()
                            {
                                go = Some(Open::Res(p.clone()));
                            }
                        };
                        if dir.is_empty() {
                            shown.into_iter().for_each(|p| leaf(ui, p));
                        } else {
                            egui::CollapsingHeader::new(format!("{dir}  ({})", shown.len()))
                                .id_salt(dir)
                                .default_open(!filter.is_empty())
                                .show(ui, |ui| shown.into_iter().for_each(|p| leaf(ui, p)));
                        }
                    }
                }
            });
        if let Some(o) = go {
            self.open(o, None, None);
        }
    }

    fn results_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.set.theme.palette();
        let mut go = None;
        let mut cancel = false;
        ui.horizontal(|ui| match (&self.prog, self.st) {
            (Some(pr), Some(_)) => {
                ui.spinner();
                let (done, total) = (
                    pr.done.load(Ordering::Relaxed),
                    pr.total.load(Ordering::Relaxed),
                );
                ui.label(format!("Searching {}…", self.hits_title));
                if total > 0 {
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total as f32)
                            .desired_width(180.0)
                            .text(format!("{done} / {total} classes")),
                    );
                }
                cancel = ui.button("Cancel").on_hover_text("Esc").clicked();
            }
            _ => {
                let more = if self.hits.len() >= session::LIMIT {
                    format!(" (first {})", session::LIMIT)
                } else {
                    String::new()
                };
                ui.label(RichText::new(&self.hits_title).strong());
                ui.label(
                    RichText::new(format!(
                        "{} results{more} in {:.1} s",
                        self.hits.len(),
                        self.hits_secs
                    ))
                    .color(p.dim),
                );
                if ui
                    .add_enabled(!self.hits.is_empty(), egui::Button::new("Export…"))
                    .on_hover_text(
                        "Save every hit with its surrounding code to .txt, .csv or .json",
                    )
                    .clicked()
                {
                    self.export = Some(ExportDlg {
                        fmt: Format::Text,
                        ctx: self.set.export_ctx,
                        method: self.set.export_method,
                    });
                }
                if ui
                    .add_enabled(!self.hits.is_empty(), egui::Button::new("Copy"))
                    .clicked()
                {
                    let all: String = self
                        .hits
                        .iter()
                        .map(|(t, ln, text)| {
                            if *ln == 0 {
                                format!("{t}\n")
                            } else {
                                format!("{t}:{ln}\t{text}\n")
                            }
                        })
                        .collect();
                    ui.ctx().copy_text(all);
                    self.toast(format!("Copied {} results", self.hits.len()));
                }
                if ui.button("Close").clicked() {
                    self.hits.clear();
                    self.hits_title.clear();
                }
            }
        });
        if cancel {
            self.cancel_search();
        }
        ui.label(
            RichText::new(self.scope.note())
                .small()
                .italics()
                .color(p.dim),
        );
        ui.separator();
        let size = self.set.code_font - 0.5;
        let row = ui.fonts(|f| f.row_height(&FontId::monospace(size))) + 4.0;
        // hits grouped by class (they arrive sorted by class): a foldable header, then its lines
        let flt = self.hits_filter.to_lowercase();
        let keep = |h: &Hit| {
            flt.is_empty() || h.0.to_lowercase().contains(&flt) || h.2.to_lowercase().contains(&flt)
        };
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (k, h) in self.hits.iter().enumerate().filter(|(_, h)| keep(h)) {
            match groups.last_mut() {
                Some(g) if g.0 == h.0 => g.1.push(k),
                _ => groups.push((&h.0, vec![k])),
            }
        }
        ui.horizontal(|ui| {
            edit(ui, &mut self.hits_filter, "Filter results…", 220.0);
            ui.label(RichText::new(format!("{} classes", groups.len())).color(p.dim));
            if ui.small_button("Fold all").clicked() {
                self.folded = groups.iter().map(|g| g.0.to_string()).collect();
            }
            if ui.small_button("Unfold all").clicked() {
                self.folded.clear();
            }
        });
        ui.separator();
        // rows: (hit index, is header)
        let mut rows: Vec<(usize, bool)> = Vec::new();
        for (c, ks) in &groups {
            rows.push((ks[0], true));
            if !self.folded.contains(*c) {
                rows.extend(
                    ks.iter()
                        .filter(|&&k| !self.hits[k].2.is_empty())
                        .map(|&k| (k, false)),
                );
            }
        }
        let counts: std::collections::HashMap<&str, usize> = groups
            .iter()
            .map(|g| {
                (
                    g.0,
                    g.1.iter().filter(|&&k| !self.hits[k].2.is_empty()).count(),
                )
            })
            .collect();
        // the query, to mark inside each line
        let needle = (!self.use_regex && !self.query.is_empty()).then(|| {
            if self.case {
                self.query.clone()
            } else {
                self.query.to_lowercase()
            }
        });
        let re = self
            .use_regex
            .then(|| {
                regex::RegexBuilder::new(&self.query)
                    .case_insensitive(!self.case)
                    .build()
                    .ok()
            })
            .flatten();
        let mut fold = None;
        let mono = FontId::monospace(size);
        egui::ScrollArea::both()
            .auto_shrink(false)
            .show_rows(ui, row, rows.len(), |ui, range| {
                for &(k, header) in &rows[range] {
                    let (t, ln, text) = &self.hits[k];
                    let mut job = LayoutJob::default();
                    if header {
                        let n = counts.get(t.as_str()).copied().unwrap_or(0);
                        let arrow = if n == 0 {
                            "  "
                        } else if self.folded.contains(t) {
                            "▶ "
                        } else {
                            "▼ "
                        };
                        job.append(arrow, 0.0, TextFormat::simple(mono.clone(), p.dim));
                        let (pkg, name) = t.rsplit_once(['.', '/']).map_or(("", t.as_str()), |x| x);
                        job.append(name, 0.0, TextFormat::simple(mono.clone(), p.accent));
                        if !pkg.is_empty() {
                            job.append(
                                &format!("  {pkg}"),
                                0.0,
                                TextFormat::simple(mono.clone(), p.dim),
                            );
                        }
                        if n > 0 {
                            job.append(
                                &format!("  ({n})"),
                                0.0,
                                TextFormat::simple(mono.clone(), p.dim),
                            );
                        }
                        let r = ui
                            .add(egui::Label::new(job).sense(egui::Sense::click()).extend())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if r.clicked() {
                            if n == 0 {
                                go = Some((t.clone(), 0))
                            } else {
                                fold = Some(t.clone())
                            }
                        }
                        if r.double_clicked() {
                            go = Some((t.clone(), 0));
                        }
                        continue;
                    }
                    let lnt = if *ln > 0 {
                        ln.to_string()
                    } else {
                        String::new()
                    };
                    job.append(
                        &format!("    {lnt:>5}  "),
                        0.0,
                        TextFormat::simple(mono.clone(), p.dim),
                    );
                    let line = text.trim();
                    // spans of the query in this line
                    let spans: Vec<(usize, usize)> = match (&needle, &re) {
                        (Some(nd), _) => {
                            let hay = if self.case {
                                line.to_string()
                            } else {
                                line.to_lowercase()
                            };
                            // lowercasing can change byte lengths; only mark when it didn't
                            if hay.len() == line.len() {
                                hay.match_indices(nd.as_str())
                                    .map(|(i, m)| (i, i + m.len()))
                                    .collect()
                            } else {
                                vec![]
                            }
                        }
                        (_, Some(re)) => re.find_iter(line).map(|m| (m.start(), m.end())).collect(),
                        _ => vec![],
                    };
                    let mut at = 0;
                    for (s0, e0) in spans {
                        job.append(&line[at..s0], 0.0, TextFormat::simple(mono.clone(), p.text));
                        let mut f = TextFormat::simple(mono.clone(), p.text);
                        f.background = p.accent.gamma_multiply(0.38);
                        job.append(&line[s0..e0], 0.0, f);
                        at = e0;
                    }
                    job.append(&line[at..], 0.0, TextFormat::simple(mono.clone(), p.text));
                    if ui
                        .add(egui::Label::new(job).sense(egui::Sense::click()).extend())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Open at this line")
                        .clicked()
                    {
                        go = Some((t.clone(), *ln));
                    }
                }
            });
        if let Some(c) = fold {
            if !self.folded.remove(&c) {
                self.folded.insert(c);
            }
        }
        if let Some((t, ln)) = go {
            let o = match self.st {
                Some(s) if s.is_native(&t) => Open::Native(t),
                Some(s) if s.is_res(&t) => Open::Res(t),
                _ => Open::Class(t),
            };
            self.push_nav();
            self.open(o, Some(View::Java), (ln > 0).then_some(ln));
            // mark the searched name in the opened code
            let q = self.query.clone();
            if let Some(d) = self.docs.get_mut(self.active) {
                if !self.use_regex
                    && !q.is_empty()
                    && q.chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                {
                    d.sel = Some(q);
                    d.job = None;
                }
            }
        }
    }

    /// "Add file", "Edit strings" and "Build & sign" windows.
    fn edit_windows(&mut self, ctx: &egui::Context) {
        let p = self.set.theme.palette();
        if let Some(dlg) = &mut self.strings_dlg {
            let (mut open, mut apply) = (true, false);
            egui::Window::new("Edit strings")
                .collapsible(false)
                .default_size([720.0, 520.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    let mut configs: Vec<&str> =
                        dlg.all.iter().map(|r| r.config.as_str()).collect();
                    configs.sort();
                    configs.dedup();
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("str-config")
                            .selected_text(if dlg.config.is_empty() {
                                "default"
                            } else {
                                &dlg.config
                            })
                            .show_ui(ui, |ui| {
                                for c in &configs {
                                    ui.selectable_value(
                                        &mut dlg.config,
                                        c.to_string(),
                                        if c.is_empty() { "default" } else { c },
                                    );
                                }
                            });
                        edit(ui, &mut dlg.filter, "Filter by name or text…", 260.0);
                        ui.label(
                            RichText::new(format!("{} changed", dlg.changes.len())).color(p.dim),
                        );
                    });
                    ui.separator();
                    let f = dlg.filter.to_lowercase();
                    let rows: Vec<usize> = dlg
                        .all
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| {
                            r.config == dlg.config
                                && (f.is_empty()
                                    || r.name.to_lowercase().contains(&f)
                                    || r.value.to_lowercase().contains(&f))
                        })
                        .map(|x| x.0)
                        .collect();
                    let h = ui.text_style_height(&egui::TextStyle::Body) + 10.0;
                    egui::ScrollArea::vertical()
                        .max_height(400.0)
                        .auto_shrink(false)
                        .show_rows(ui, h, rows.len(), |ui, range| {
                            for &k in &rows[range] {
                                let r = &dlg.all[k];
                                let key = (r.config.clone(), r.name.clone());
                                let mut text = dlg
                                    .changes
                                    .get(&key)
                                    .cloned()
                                    .unwrap_or_else(|| r.value.clone());
                                ui.horizontal(|ui| {
                                    let changed = dlg.changes.contains_key(&key);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(220.0, h - 6.0),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.set_width(220.0);
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&r.name).monospace().color(
                                                        if changed { p.accent } else { p.text },
                                                    ),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(&r.name);
                                        },
                                    );
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut text)
                                                .desired_width(f32::INFINITY),
                                        )
                                        .changed()
                                    {
                                        if text == r.value {
                                            dlg.changes.remove(&key);
                                        } else {
                                            dlg.changes.insert(key.clone(), text.clone());
                                        }
                                    }
                                });
                            }
                        });
                    ui.separator();
                    apply = ui
                        .add_enabled(
                            !dlg.changes.is_empty(),
                            egui::Button::new(RichText::new("Apply").strong())
                                .fill(p.accent.gamma_multiply(0.55)),
                        )
                        .clicked();
                });
            if apply {
                let dlg = self.strings_dlg.take().unwrap();
                match arsc::set_strings(&dlg.base, &dlg.changes) {
                    Ok(bytes) => {
                        self.edits.insert("resources.arsc".into(), Some(bytes));
                        self.toast(format!(
                            "{} string(s) will change when you build (Files tab)",
                            dlg.changes.len()
                        ));
                    }
                    Err(e) => self.toast(format!("Strings not applied: {e}")),
                }
            } else if !open {
                self.strings_dlg = None;
            }
        }
        if let Some((path, data)) = &mut self.adding {
            let (mut open, mut add) = (true, false);
            egui::Window::new("Add file")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "{} to add. Path inside the APK:",
                        mb(data.len() as u64)
                    ));
                    ui.add(egui::TextEdit::singleline(path).desired_width(360.0));
                    ui.label(
                        RichText::new(
                            "assets/ is safe for any file; lib/<abi>/ for native libraries.",
                        )
                        .small()
                        .color(p.dim),
                    );
                    add = ui
                        .add_enabled(
                            !path.trim().is_empty() && !path.contains(".."),
                            egui::Button::new("Add"),
                        )
                        .clicked();
                });
            if add {
                let (path, data) = self.adding.take().unwrap();
                let path = path.trim().trim_start_matches('/').replace('\\', "/");
                self.toast(format!("{path} will be added when you build"));
                self.edits.insert(path, Some(data));
            } else if !open {
                self.adding = None;
            }
        }
        let Some(dlg) = &mut self.build else { return };
        let Some(st) = self.st else { return };
        let (mut open, mut go) = (true, false);
        egui::Window::new("Build & sign").collapsible(false).resizable(false).open(&mut open).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            let smali_n = self.smali_edits.len();
            let smali_note = if smali_n > 0 { format!(" (incl. {smali_n} edited smali class(es), assembled on build)") } else { String::new() };
            ui.label(format!("{} change(s){smali_note}. The new APK is zipaligned and signed with APK Signature Scheme v2 + v3.", self.edits.len() + smali_n));
            ui.add_space(6.0);
            ui.radio_value(&mut dlg.own_key, false, "apkscope's key (created on first use)");
            ui.radio_value(&mut dlg.own_key, true, "My own key");
            if dlg.own_key {
                for (label, field, ext) in [("Private key (PKCS#8 .pk8 / .pem)", &mut dlg.key, ["pk8", "pem", "der"]), ("Certificate (.crt / .pem / .der)", &mut dlg.cert, ["crt", "pem", "der"])] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        if ui.button("Choose…").clicked() {
                            if let Some(f) = rfd::FileDialog::new().add_filter(label, &ext).pick_file() {
                                *field = f.display().to_string();
                            }
                        }
                    });
                    if !field.is_empty() {
                        ui.label(RichText::new(field.as_str()).small().color(p.dim));
                    }
                }
            }
            ui.add_space(6.0);
            let warn = Color32::from_rgb(230, 160, 60);
            ui.colored_label(warn, "A different signature means it won't install as an update over the original app: uninstall that first.");
            if st.min_sdk().is_some_and(|m| m < 24) {
                ui.colored_label(warn, format!("This app supports Android below 7.0 (minSdk {}). v2/v3-only APKs install on Android 7.0+.", st.min_sdk().unwrap_or(0)));
            }
            ui.add_space(6.0);
            let ready = !dlg.own_key || (!dlg.key.is_empty() && !dlg.cert.is_empty());
            go = ui.add_enabled(ready, egui::Button::new(RichText::new("Build…").strong()).fill(p.accent.gamma_multiply(0.55))).clicked();
        });
        if go {
            let stem = std::path::Path::new(&st.path)
                .file_stem()
                .map_or("app".into(), |s| s.to_string_lossy().into_owned());
            if let Some(out) = rfd::FileDialog::new()
                .set_file_name(format!("{stem}-apkscope.apk"))
                .add_filter("APK", &["apk"])
                .save_file()
            {
                let own = dlg.own_key.then(|| (dlg.key.clone(), dlg.cert.clone()));
                let (tx, mut edits, file) = (self.tx.clone(), self.edits.clone(), st.file.clone());
                let smali_srcs = self.smali_edits.clone();
                spawn(&self.tx, "save", move || {
                    let r = (|| {
                        let key = match own {
                            Some((k, c)) => {
                                apksign::load(std::path::Path::new(&k), std::path::Path::new(&c))?
                            }
                            None => apksign::default_key()?.0,
                        };
                        if !smali_srcs.is_empty() {
                            smali::inject(&file, &smali_srcs, &mut edits)?;
                        }
                        let (apk, cd) = rebuild::rebuild(&file, &edits)?;
                        let signed = apksign::sign(apk, cd, &key)?;
                        std::fs::write(&out, &signed)
                            .map_err(|e| format!("{}: {e}", out.display()))?;
                        let check = apksign::verify(&out.display().to_string())?;
                        Ok(format!(
                            "Built {} ({}). {}",
                            out.display(),
                            mb(signed.len() as u64),
                            check.join(", ")
                        ))
                    })();
                    let _ = tx.send(Msg::Saved(r));
                });
                self.toast("Building…");
            }
            self.build = None;
        } else if !open {
            self.build = None;
        }
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.export else { return };
        let mut open = true;
        let mut save = false;
        egui::Window::new("Export results")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("{} hits from {}", self.hits.len(), self.hits_title));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Format");
                    ui.radio_value(&mut dlg.fmt, Format::Text, "Text");
                    ui.radio_value(&mut dlg.fmt, Format::Csv, "CSV");
                    ui.radio_value(&mut dlg.fmt, Format::Json, "JSON");
                });
                ui.checkbox(&mut dlg.method, "Whole enclosing method for each hit");
                ui.add_enabled_ui(!dlg.method, |ui| {
                    ui.add(
                        egui::Slider::new(&mut dlg.ctx, 0..=30)
                            .text("lines of context around each hit"),
                    );
                });
                ui.add_space(6.0);
                save = ui.button("Save…").clicked();
            });
        if save {
            let (fmt, ctxl, method) = (dlg.fmt, dlg.ctx, dlg.method);
            let ext = match fmt {
                Format::Text => "txt",
                Format::Csv => "csv",
                Format::Json => "json",
            };
            if let Some(file) = rfd::FileDialog::new()
                .set_file_name(format!("apkscope-results.{ext}"))
                .add_filter(ext, &[ext])
                .save_file()
            {
                let (tx, hits, st) = (self.tx.clone(), self.hits.clone(), self.st);
                spawn(&self.tx, "save", move || {
                    let Some(st) = st else { return };
                    let out = session::export(&hits, ctxl, method, fmt, |t| st.target_text(t));
                    let r = std::fs::write(&file, out)
                        .map(|_| format!("Saved {} hits to {}", hits.len(), file.display()))
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Saved(r));
                });
                self.set.export_ctx = ctxl;
                self.set.export_method = method;
                self.set.save();
            }
            self.export = None;
        } else if !open {
            self.export = None;
        }
    }

    /// Before an APK is open: logo, a drop zone and what apkscope accepts.
    fn start_screen(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let logo = self.logo.get_or_insert_with(|| {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [64, 64],
                include_bytes!("../assets/icon64.rgba"),
            );
            ui.ctx()
                .load_texture("logo", img, egui::TextureOptions::LINEAR)
        });
        let dragging = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let loading = !self.path.is_empty() && self.err.is_none();
        let mut pick = false;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.12).max(10.0));
            ui.add(egui::Image::new(&*logo).fit_to_exact_size(egui::vec2(96.0, 96.0)));
            ui.add_space(6.0);
            ui.label(RichText::new("apkscope").size(30.0).strong().color(p.text));
            ui.label(RichText::new("Android decompiler and API recon").size(15.0).color(p.dim));
            ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).size(12.0).color(p.dim.gamma_multiply(0.8)));
            ui.add_space(24.0);
            let stroke = if dragging { Stroke::new(2.5_f32, p.accent) } else { Stroke::new(1.5_f32, p.dim.gamma_multiply(0.6)) };
            let fill = if dragging { p.accent.gamma_multiply(0.12) } else { p.panel };
            egui::Frame::new().fill(fill).stroke(stroke).corner_radius(CornerRadius::same(16)).inner_margin(Margin::symmetric(60, 36)).show(ui, |ui| {
                ui.set_width(520.0_f32.min(ui.available_width()));
                ui.vertical_centered(|ui| {
                    if loading {
                        ui.spinner();
                        ui.label(RichText::new(format!("Opening {}…", self.path)).size(15.0));
                    } else {
                        ui.label(RichText::new(if dragging { "Release to open" } else { "Drop an APK here" }).size(22.0).strong().color(p.text));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Also opens split bundles (.apks, .xapk) and .dex files").color(p.dim));
                        ui.add_space(14.0);
                        pick = ui.add(egui::Button::new(RichText::new("Open file…").size(15.0).strong()).fill(p.accent.gamma_multiply(0.55)).min_size(egui::vec2(160.0, 34.0))).clicked();
                    }
                });
            });
            if let Some(e) = &self.err {
                ui.add_space(12.0);
                ui.colored_label(Color32::from_rgb(230, 80, 80), format!("Couldn't open it: {e}"));
            }
            ui.add_space(22.0);
            ui.label(RichText::new("Decompile to Java  ·  find URLs, API calls and keys  ·  inspect native libraries  ·  export a Gradle project").color(p.dim));
        });
        if pick {
            if let Some(f) = rfd::FileDialog::new()
                .add_filter("APK, split bundle or DEX", &session::INPUTS)
                .pick_file()
            {
                self.err = None;
                self.start_load(f.display().to_string());
            }
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let before = self.set.clone();
        let st = self.st;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .default_width(420.0)
            .show(ctx, |ui| {
                let s = &mut self.set;
                ui.heading("Appearance");
                egui::Grid::new("app")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Theme");
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(s.theme.name())
                            .show_ui(ui, |ui| {
                                for t in Theme::ALL {
                                    ui.selectable_value(&mut s.theme, t, t.name());
                                }
                            });
                        ui.end_row();
                        ui.label("Code font size");
                        ui.add(egui::Slider::new(&mut s.code_font, 10.0..=22.0).step_by(0.5));
                        ui.end_row();
                        ui.label("Interface scale");
                        ui.add(egui::Slider::new(&mut s.ui_scale, 0.8..=1.6).step_by(0.05));
                        ui.end_row();
                    });
                ui.separator();
                ui.heading("Decompiler");
                ui.checkbox(
                    &mut s.deobf,
                    "Rename obfuscated classes, methods and fields",
                )
                .on_hover_text("Applies to the next APK you open");
                ui.horizontal(|ui| {
                    ui.label("Default export context");
                    ui.add(egui::Slider::new(&mut s.export_ctx, 0..=30).text("lines"));
                });
                ui.separator();
                ui.heading("Performance");
                let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
                ui.horizontal(|ui| {
                    ui.label("Worker threads");
                    ui.add(
                        egui::Slider::new(&mut s.threads, 0..=cores * 2).custom_formatter(
                            |v, _| {
                                if v == 0.0 {
                                    format!("auto ({cores})")
                                } else {
                                    format!("{v}")
                                }
                            },
                        ),
                    );
                });
                ui.checkbox(
                    &mut s.use_cache,
                    "Cache decompiled code on disk (fast repeat searches)",
                );
                ui.separator();
                ui.heading("Cache & usage");
                let size = *self.cache_size.get_or_insert_with(cache::total_size);
                egui::Grid::new("usage")
                    .num_columns(2)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Memory in use");
                        ui.label(session::memory_used().map_or("?".into(), mb));
                        ui.end_row();
                        ui.label("Disk cache (all APKs)");
                        ui.label(mb(size));
                        ui.end_row();
                        if let Some(st) = st {
                            ui.label("This APK");
                            let idx = if st.index.get().is_some() {
                                "indexed".to_string()
                            } else {
                                format!(
                                    "indexing {} / {}",
                                    st.indexed.load(Ordering::Relaxed),
                                    st.classes.len()
                                )
                            };
                            ui.label(format!(
                                "{idx}, {}",
                                if st.cached() {
                                    "code cached"
                                } else {
                                    "code not cached yet"
                                }
                            ));
                            ui.end_row();
                        }
                        ui.label("Cache folder");
                        ui.label(RichText::new(cache::root().display().to_string()).small());
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Clear cache").clicked() {
                        let _ = cache::clear_all();
                        if let Some(st) = st {
                            st.drop_cache();
                        }
                        self.cache_size = None;
                    }
                    if ui.button("Open folder").clicked() {
                        let _ = std::fs::create_dir_all(cache::root());
                        #[cfg(windows)]
                        let _ = std::process::Command::new("explorer")
                            .arg(cache::root())
                            .spawn();
                    }
                });
            });
        if self.set.theme != before.theme || self.set.ui_scale != before.ui_scale {
            self.styled = None;
        }
        if self.set.code_font != before.code_font || self.set.theme != before.theme {
            self.docs.iter_mut().for_each(|d| d.job = None);
        }
        session::THREADS.store(self.set.threads, Ordering::Relaxed);
        session::CACHE_WRITE.store(self.set.use_cache, Ordering::Relaxed);
        if !open {
            self.show_settings = false;
            self.cache_size = None;
            self.set.save();
        }
    }
}

fn node(
    ui: &mut egui::Ui,
    t: &Tree,
    prefix: &str,
    doc: &Option<Open>,
    clicked: &mut Option<String>,
) {
    for (name, sub) in &t.dirs {
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        egui::CollapsingHeader::new(name.as_str())
            .id_salt(&full)
            .show(ui, |ui| node(ui, sub, &full, doc, clicked));
    }
    for (short, full) in &t.leaves {
        if ui
            .selectable_label(doc.as_ref() == Some(&Open::Class(full.clone())), short)
            .clicked()
        {
            *clicked = Some(full.clone());
        }
    }
}

const KEYWORDS: &[&str] = &[
    "abstract",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
];

/// Java/Smali/XML colouring: comments, strings, keywords, numbers, smali directives.
/// `mark` (1-based) gets a background, for jumping to a search hit.
/// `sel` = identifier to mark everywhere (the word the user clicked).
fn highlight(
    text: &str,
    p: &Palette,
    size: f32,
    mark: Option<usize>,
    sel: Option<&str>,
) -> LayoutJob {
    let font = FontId::monospace(size);
    let mut job = LayoutJob::default();
    let mut line = 1;
    let mut push = |job: &mut LayoutJob, s: &str, k: u8| {
        // split at newlines so only the marked line gets the background
        for piece in s.split_inclusive('\n') {
            let mut f = TextFormat::simple(font.clone(), p.code[(k % 6) as usize]);
            if Some(line) == mark {
                f.background = p.mark;
            }
            // k >= 6: the selected word
            if k >= 6 {
                f.background = p.accent.gamma_multiply(0.38);
            }
            job.append(piece, 0.0, f);
            if piece.ends_with('\n') {
                line += 1;
            }
        }
    };
    let b = text.as_bytes();
    let (mut i, mut start) = (0, 0);
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    while i < b.len() {
        let c = b[i];
        let tok_end = if text[i..].starts_with("/*") {
            let e = text[i + 2..].find("*/").map_or(b.len(), |p| i + 4 + p);
            Some((e, if text[i..e].contains("WARNING") { 5 } else { 1 }))
        } else if text[i..].starts_with("//") || (c == b'#' && (i == 0 || b[i - 1] == b'\n')) {
            Some((text[i..].find('\n').map_or(b.len(), |p| i + p), 1))
        } else if c == b'"' {
            let mut e = i + 1;
            while e < b.len() && b[e] != b'"' && b[e] != b'\n' {
                e += if b[e] == b'\\' { 2 } else { 1 };
            }
            Some(((e + 1).min(b.len()), 2))
        } else if c == b'.'
            && (i == 0 || !word(b[i - 1]))
            && b.get(i + 1).is_some_and(|c| c.is_ascii_lowercase())
        {
            let e = i
                + 1
                + b[i + 1..]
                    .iter()
                    .position(|&c| !word(c))
                    .unwrap_or(b.len() - i - 1);
            Some((e, 5))
        } else if word(c) && (i == 0 || !word(b[i - 1])) {
            let e = i + b[i..].iter().position(|&c| !word(c)).unwrap_or(b.len() - i);
            let w = &text[i..e];
            if Some(w) == sel {
                Some((e, 6))
            } else if KEYWORDS.contains(&w) {
                Some((e, 3))
            } else if c.is_ascii_digit() {
                Some((e, 4))
            } else {
                i = e;
                continue;
            }
        } else {
            None
        };
        match tok_end {
            Some((e, k)) => {
                if start < i {
                    push(&mut job, &text[start..i], 0);
                }
                push(&mut job, &text[i..e], k);
                i = e;
                start = e;
            }
            None => i += text[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if start < b.len() {
        push(&mut job, &text[start..], 0);
    }
    job
}

/// Next line after `from` (1-based) containing `needle`, wrapping around.
fn find_next(text: &str, needle: &str, from: usize) -> Option<usize> {
    let low = needle.to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    (0..n)
        .map(|k| (from + k) % n)
        .find(|&k| lines[k].to_lowercase().contains(&low))
        .map(|k| k + 1)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.style(ctx);
        if let (Some(st), false) = (self.st, self.titled) {
            let name = std::path::Path::new(&st.path)
                .file_name()
                .map_or(st.path.clone(), |n| n.to_string_lossy().into_owned());
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} - apkscope")));
            self.titled = true;
        }
        let p = self.set.theme.palette();
        // files dropped on the window
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.start_load(path.display().to_string());
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.focus_search = true;
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W))
            && !self.docs.is_empty()
        {
            self.close(self.active);
        }
        let (back, fwd) = ctx.input(|i| {
            (
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)
                    || i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)
                    || i.pointer.button_pressed(egui::PointerButton::Extra2),
            )
        });
        if back {
            self.nav(false);
        } else if fwd {
            self.nav(true);
        }
        // Escape cancels a running search, else clears the marked word
        if self.prog.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_search();
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if let Some(d) = self.docs.get_mut(self.active) {
                if d.sel.take().is_some() {
                    d.job = None;
                }
            }
        }
        // Ctrl+Tab / Ctrl+Shift+Tab switch tabs, Ctrl+O opens an APK
        let (next_tab, prev_tab, open_apk) = ctx.input(|i| {
            (
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Tab),
                i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::Tab),
                i.modifiers.command && i.key_pressed(egui::Key::O),
            )
        });
        if !self.docs.is_empty() && (next_tab || prev_tab) {
            let n = self.docs.len();
            self.active = if next_tab {
                (self.active + 1) % n
            } else {
                (self.active + n - 1) % n
            };
        }
        egui::TopBottomPanel::top("bar").frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(10, 8))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("apkscope").strong().size(16.0).color(p.accent));
                ui.add_space(6.0);
                if ui.button("Open…").on_hover_text("Open an APK, split bundle or DEX (Ctrl+O)").clicked() || open_apk {
                    if let Some(f) = rfd::FileDialog::new().add_filter("APK, split bundle or DEX", &session::INPUTS).pick_file() {
                        self.start_load(f.display().to_string());
                    }
                }
                // one-off jobs live in a menu so the bar stays readable
                ui.add_enabled_ui(self.st.is_some(), |ui| {
                    ui.menu_button("Tools", |ui| {
                        if ui.button("Export Gradle project…").on_hover_text("Write a Gradle project (runs in the background)").clicked() {
                            ui.close_menu();
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                if let Ok(exe) = std::env::current_exe() {
                                    let mut cmd = std::process::Command::new(exe);
                                    cmd.arg("export").arg(&self.path).arg(&dir);
                                    if !self.set.deobf {
                                        cmd.arg("--no-deobf");
                                    }
                                    let _ = cmd.spawn();
                                    self.toast(format!("Exporting to {} in the background", dir.display()));
                                }
                            }
                        }
                        if ui.button("Security scan").on_hover_text("Manifest flags, exported components, risky APIs, weak crypto, TLS overrides, keys and URLs").clicked() {
                            self.report("Security scan".into(), |st| crate::re::scan(st).lines().map(|l| (l.to_string(), None)).collect());
                            ui.close_menu();
                        }
                        if ui.button("Decrypt strings").on_hover_text("Run string-decryptor methods on their constant arguments (also shown inline in the code as /* = \"...\" */)").clicked() {
                            self.report("Decrypted strings".into(), |st| crate::re::decrypt_report(&st.dexes));
                            ui.close_menu();
                        }
                    });
                });
                ui.separator();
                let scope = self.scope.label();
                egui::ComboBox::from_id_salt("scope").width(130.0).selected_text(scope).show_ui(ui, |ui| {
                    for s in Scope::ALL {
                        ui.selectable_value(&mut self.scope, s, s.label()).on_hover_text(s.note());
                    }
                })
                .response
                .on_hover_text("Search scope (Up / Down in the search box)")
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, format!("Search scope: {scope}")));
                let hint = if self.scope.query_optional() { "optional filter (Enter lists all)" } else { "Search…  (Ctrl+F)" };
                let r = edit(ui, &mut self.query, hint, (ui.available_width() - 470.0).clamp(140.0, 520.0));
                if std::mem::take(&mut self.focus_search) {
                    r.request_focus();
                }
                // Up / Down in the search box change the scope
                if r.has_focus() {
                    let (up, down) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown)));
                    if up || down {
                        let k = Scope::ALL.iter().position(|&s| s == self.scope).unwrap_or(0);
                        let n = Scope::ALL.len();
                        self.scope = Scope::ALL[if down { (k + 1) % n } else { (k + n - 1) % n }];
                    }
                }
                let (case, re) = (self.case, self.use_regex);
                ui.toggle_value(&mut self.case, "Aa").on_hover_text("Match case").widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, case, "Match case"));
                ui.toggle_value(&mut self.use_regex, ".*").on_hover_text("Regular expression").widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, re, "Regular expression"));
                ui.toggle_value(&mut self.app_only, "App only").on_hover_text("Skip classes from known libraries and SDKs (faster, less noise)");
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let go = ui.add_enabled(self.st.is_some(), egui::Button::new(RichText::new("Search").strong()).fill(p.accent.gamma_multiply(0.55)));
                if go.clicked() || enter {
                    self.run_search();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").on_hover_text("Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    // Frida costs nothing until this is on
                    let on = self.live.open;
                    let label = RichText::new(if self.live.running() { "● Live  on" } else { "○ Live" }).strong().color(if on { Color32::BLACK } else { p.dim });
                    let b = egui::Button::new(label).fill(if on { Color32::from_rgb(60, 255, 120) } else { p.panel }).corner_radius(CornerRadius::same(10));
                    if ui.add_enabled(self.st.is_some(), b).on_hover_text("Live mode: run the app under Frida, hook methods with a click, see what runs").clicked() {
                        self.live.open = !on;
                    }
                });
            });
            if let Some(e) = &self.search_err {
                ui.colored_label(Color32::from_rgb(230, 80, 80), e.lines().last().unwrap_or(e));
            }
            if self.scope.query_optional() {
                ui.label(RichText::new(self.scope.note()).small().italics().color(p.dim));
            }
        });
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(10, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| match (self.st, &self.err) {
                    (_, Some(e)) => {
                        ui.colored_label(Color32::RED, format!("Failed to open: {e}"));
                    }
                    (Some(st), _) => {
                        ui.label(
                            RichText::new(format!(
                                "{}  ·  {} classes  ·  {} methods  ·  {} dex  ·  {} resources",
                                st.package,
                                st.classes.len(),
                                st.methods,
                                st.dexes.len(),
                                st.texts.len() + st.files.len()
                            ))
                            .color(p.dim),
                        );
                        ui.separator();
                        match st.index.get() {
                            Some(_) => ui.label(RichText::new("indexed").color(p.dim)),
                            None => {
                                ui.spinner();
                                ui.label(
                                    RichText::new(format!(
                                        "indexing {} / {}",
                                        st.indexed.load(Ordering::Relaxed),
                                        st.classes.len()
                                    ))
                                    .color(p.dim),
                                )
                            }
                        };
                        if st.cached() {
                            ui.label(RichText::new("·  code cached").color(p.dim));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(m) = session::memory_used() {
                                ui.label(RichText::new(mb(m)).color(p.dim));
                            }
                        });
                    }
                    (None, _) if !self.path.is_empty() => {
                        ui.spinner();
                        ui.label(format!("Loading {}…", self.path));
                    }
                    _ => {
                        ui.label("Open an APK: the button above, or drop it on the window.");
                    }
                });
            });
        // the sidebar only means something once an APK is open
        if self.st.is_some() {
            egui::SidePanel::left("side")
                .default_width(330.0)
                .resizable(true)
                .frame(
                    egui::Frame::new()
                        .fill(p.panel)
                        .inner_margin(Margin::same(8)),
                )
                .show(ctx, |ui| self.side_panel(ui));
        }
        if let (Some(st), true) = (self.st, self.live.open) {
            let mut act = None;
            egui::SidePanel::right("live")
                .resizable(true)
                .default_width(440.0)
                .min_width(320.0)
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(6, 10, 8))
                        .inner_margin(Margin::same(8)),
                )
                .show(ctx, |ui| act = self.live.show(ui, st));
            if let Some(a) = act {
                self.act(a, ctx);
            }
        }
        if self.prog.is_some() || !self.hits_title.is_empty() {
            egui::TopBottomPanel::bottom("hits")
                .resizable(true)
                .default_height(240.0)
                .frame(
                    egui::Frame::new()
                        .fill(p.panel)
                        .inner_margin(Margin::same(8)),
                )
                .show(ctx, |ui| self.results_panel(ui));
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::same(8))).show(ctx, |ui| {
            if self.st.is_none() {
                self.start_screen(ui, &p);
                return;
            }
            if self.docs.is_empty() {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Pick a class or resource on the left.").size(16.0).color(p.dim)));
                return;
            }
            // tab strip
            let mut close = None;
            let mut pick = None;
            // (which tabs to close, relative to tab k): 0 others, 1 left, 2 right, 3 all
            let mut close_many: Option<(usize, u8)> = None;
            let mut go_nav = None;
            egui::ScrollArea::horizontal().id_salt("tabs").show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.back.is_empty(), egui::Button::new("◀").frame(false)).on_hover_text("Back (Alt+Left, mouse back)").clicked() {
                        go_nav = Some(false);
                    }
                    if ui.add_enabled(!self.fwd.is_empty(), egui::Button::new("▶").frame(false)).on_hover_text("Forward (Alt+Right)").clicked() {
                        go_nav = Some(true);
                    }
                    ui.separator();
                    let n = self.docs.len();
                    for (k, d) in self.docs.iter().enumerate() {
                        let active = k == self.active;
                        let fill = if active { p.panel } else { Color32::TRANSPARENT };
                        egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(6)).inner_margin(Margin::symmetric(8, 3)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let t = RichText::new(d.open.short()).color(if active { p.text } else { p.dim });
                                let r = ui.add(egui::Label::new(t).sense(egui::Sense::click())).on_hover_text(d.open.name());
                                if r.clicked() {
                                    pick = Some(k);
                                }
                                if r.middle_clicked() || ui.add(egui::Button::new(RichText::new("x").small().color(p.dim)).frame(false)).on_hover_text("Close (Ctrl+W, middle-click)").clicked() {
                                    close = Some(k);
                                }
                                r.context_menu(|ui| {
                                    for (label, what, on) in [("Close", 9, true), ("Close others", 0, n > 1), ("Close tabs to the left", 1, k > 0), ("Close tabs to the right", 2, k + 1 < n), ("Close all", 3, true)] {
                                        if ui.add_enabled(on, egui::Button::new(label)).clicked() {
                                            if what == 9 { close = Some(k) } else { close_many = Some((k, what)) }
                                            ui.close_menu();
                                        }
                                    }
                                    ui.separator();
                                    if ui.button("Copy name").clicked() {
                                        ui.ctx().copy_text(d.open.name().to_string());
                                        ui.close_menu();
                                    }
                                });
                            });
                        });
                    }
                });
            });
            if let Some(k) = pick {
                self.active = k;
            }
            if let Some(f) = go_nav {
                self.nav(f);
                return;
            }
            if let Some((k, what)) = close_many {
                let keep = |i: usize| match what {
                    0 => i == k,
                    1 => i >= k,
                    2 => i <= k,
                    _ => false,
                };
                let mut i = 0;
                self.docs.retain(|_| { let r = keep(i); i += 1; r });
                self.active = match what {
                    0 | 1 => 0,
                    2 => k,
                    _ => 0,
                }
                .min(self.docs.len().saturating_sub(1));
                return;
            }
            if let Some(k) = close {
                self.close(k);
                return;
            }
            let (pal, size) = (p, self.set.code_font);
            let mut switch = None;
            let mut usages = None;
            let mut toast_msg = None;
            let mut change: Option<(String, Option<Vec<u8>>)> = None;
            let mut stage_smali: Option<String> = None;
            let mut act: Option<Act> = None;
            let st_now = self.st;
            let Some(doc) = self.docs.get_mut(self.active) else { return };
            ui.horizontal(|ui| {
                ui.label(RichText::new(doc.open.name()).strong());
                if let Open::Class(c) = &doc.open {
                    ui.separator();
                    for (v, name) in [(View::Java, "Java"), (View::Smali, "Smali")] {
                        if ui.selectable_label(doc.view == v, name).clicked() && doc.view != v {
                            switch = Some(v);
                        }
                    }
                    if ui.button("Usages").on_hover_text("Find where this class is used").clicked() {
                        usages = Some(c.rsplit('.').next().unwrap_or(c).to_string());
                    }
                }
                // edit a class as round-trippable Smali and stage it for the next Build & sign
                if matches!(&doc.open, Open::Class(_)) {
                    ui.separator();
                    match &doc.editing {
                        None => {
                            if ui.button("Edit smali").on_hover_text("Edit as round-trippable Smali; Apply stages it for the next Build & sign").clicked() {
                                let cls = doc.open.name().to_string();
                                match st_now.map(|st| smali::export_class(&st.file, &cls)) {
                                    Some(Ok(t)) => {
                                        doc.editing = Some(t);
                                        doc.view = View::Smali;
                                    }
                                    Some(Err(e)) => toast_msg = Some(format!("Can't load smali: {e}")),
                                    None => {}
                                }
                            }
                        }
                        Some(buf) => {
                            if ui.button(RichText::new("Apply smali").strong()).on_hover_text("Stage this class for the next Build & sign (assembled when you build)").clicked() {
                                stage_smali = Some(buf.clone());
                                doc.editing = None;
                            }
                            if ui.button("Cancel").clicked() {
                                doc.editing = None;
                            }
                        }
                    }
                }
                let mut extract = None;
                if let Open::Native(path) | Open::File(path) = &doc.open {
                    ui.separator();
                    if ui.button("Extract…").on_hover_text("Save this file from the APK").clicked() {
                        extract = Some(path.clone());
                    }
                    if ui.button("Replace…").on_hover_text("Use another file in its place when you build").clicked() {
                        if let Some(f) = rfd::FileDialog::new().pick_file() {
                            match std::fs::read(&f) {
                                Ok(b) => change = Some((path.clone(), Some(b))),
                                Err(e) => toast_msg = Some(format!("Can't read {}: {e}", f.display())),
                            }
                        }
                    }
                    if ui.button("Delete").on_hover_text("Leave this file out when you build").clicked() {
                        change = Some((path.clone(), None));
                    }
                }
                // the manifest can be edited as text and goes back in as binary XML
                if doc.open.name() == "AndroidManifest.xml" {
                    ui.separator();
                    match &doc.editing {
                        None => {
                            if ui.button("Edit manifest").on_hover_text("Edit as XML; Apply puts it into the next Build & sign").clicked() {
                                doc.editing = st_now.and_then(|st| st.resource("AndroidManifest.xml"));
                            }
                        }
                        Some(buf) => {
                            if ui.button(RichText::new("Apply").strong()).on_hover_text("Encode to binary XML and add it to the pending changes").clicked() {
                                match st_now.map(|st| st.encode_manifest(buf)) {
                                    Some(Ok(bin)) => {
                                        change = Some(("AndroidManifest.xml".into(), Some(bin)));
                                        doc.editing = None;
                                    }
                                    Some(Err(e)) => toast_msg = Some(format!("Manifest not applied: {e}")),
                                    None => {}
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                doc.editing = None;
                            }
                        }
                    }
                }
                if let (Some(path), Some(st)) = (extract, st_now) {
                    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                    if let Some(f) = rfd::FileDialog::new().set_file_name(&name).save_file() {
                        let r = st.read_entry(&path).ok_or("can't read it".to_string()).and_then(|b| std::fs::write(&f, b).map_err(|e| e.to_string()));
                        toast_msg = Some(match r {
                            Ok(()) => format!("Saved {}", f.display()),
                            Err(e) => format!("Extract failed: {e}"),
                        });
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(t) = &doc.text {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(t.to_string());
                        }
                        let r = edit(ui, &mut doc.find, "Find in file (Enter)", 200.0);
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !doc.find.is_empty() {
                            if let Some(l) = find_next(t, &doc.find, doc.mark.unwrap_or(0)) {
                                doc.mark = Some(l);
                                doc.scroll_pending = true;
                            }
                            r.request_focus();
                        }
                    }
                });
            });
            ui.separator();
            if let Some(buf) = &mut doc.editing {
                egui::ScrollArea::both().id_salt("manifest-edit").auto_shrink(false).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(buf).code_editor().font(FontId::monospace(size)).desired_width(f32::INFINITY).desired_rows(40));
                });
            } else if let Some(text) = doc.text.clone() {
            let key = (pal.bg, (size * 10.0) as u32, doc.mark, doc.sel.clone());
            if doc.job.as_ref().is_none_or(|j| j.0 != key) {
                doc.job = Some((key, highlight(&text, &pal, size, doc.mark, doc.sel.as_deref())));
            }
            let job = doc.job.as_ref().unwrap().1.clone();
            let mut area = egui::ScrollArea::both().id_salt(doc.open.name()).auto_shrink(false);
            if std::mem::take(&mut doc.scroll_pending) {
                if let Some(ln) = doc.mark {
                    let row = ui.fonts(|f| f.row_height(&FontId::monospace(size)));
                    area = area.vertical_scroll_offset(((ln as f32 - 6.0).max(0.0)) * row);
                }
            }
            let row = ui.fonts(|f| f.row_height(&FontId::monospace(size)));
            let glyph = ui.fonts(|f| f.glyph_width(&FontId::monospace(size), 'M'));
            let is_report = matches!(doc.open, Open::Report(_));
            area.show(ui, |ui| {
                let r = ui.add(egui::Label::new(job).extend().selectable(true));
                let at = r.interact_pointer_pos().or(r.hover_pos()).map(|p| (((p.y - r.rect.top()) / row).max(0.0) as usize, ((p.x - r.rect.left()) / glyph).max(0.0) as usize));
                let link_here = |l: usize| doc.links.as_ref().and_then(|v| v.get(l).cloned().flatten());
                if is_report && r.hovered() && r.hover_pos().is_some_and(|p| link_here(((p.y - r.rect.top()) / row).max(0.0) as usize).is_some()) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if r.secondary_clicked() {
                    doc.menu_at = at;
                }
                if r.clicked() {
                    if let Some((l, c)) = at {
                        let ctrl = ui.input(|i| i.modifiers.command);
                        if is_report {
                            // a click on a linked report line jumps to that code
                            if let Some(link) = link_here(l) {
                                act = Some(Act::GoTo(link));
                            }
                        } else if let (true, Open::Class(top), Some(st)) = (ctrl, &doc.open, st_now) {
                            act = definition(st, top, &text, l, c);
                        }
                        // a click on a name marks it everywhere in the file; again to clear
                        let w = word_at(&text, l, c).map(|w| w.0).filter(|w| !KEYWORDS.contains(&w.as_str()));
                        doc.sel = if doc.sel == w { None } else { w };
                        doc.job = None;
                    }
                }
                r.context_menu(|ui| {
                    if let Some(a) = line_menu(ui, doc, &text, st_now) {
                        act = Some(a);
                        ui.close_menu();
                    }
                });
            });
            // keyboard, with no text box focused: Up/Down move the marked line, F12 goes to
            // the definition (or follows a report link), Shift+F10 opens the line's actions
            if ui.ctx().memory(|m| m.focused().is_none()) && !doc.kb_menu {
                let (up, down, f12, menu) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::F12), i.modifiers.shift && i.key_pressed(egui::Key::F10)));
                if up || down {
                    let (cur, n) = (doc.mark.unwrap_or(0), text.lines().count().max(1));
                    doc.mark = Some(if up { cur.saturating_sub(1).max(1) } else { (cur + 1).min(n) });
                    doc.scroll_pending = true;
                    doc.job = None;
                }
                if let Some(l) = doc.mark.map(|m| m - 1).filter(|_| f12 || menu) {
                    let line = text.lines().nth(l).unwrap_or("");
                    // the name before the first `(`, else the first word
                    let col = line.find('(').and_then(|p| p.checked_sub(1)).unwrap_or(line.len() - line.trim_start().len());
                    if f12 && is_report {
                        act = doc.links.as_ref().and_then(|v| v.get(l).cloned().flatten()).map(Act::GoTo);
                    } else if let (true, Open::Class(top), Some(st)) = (f12, &doc.open, st_now) {
                        act = definition(st, top, &text, l, col);
                    }
                    if menu {
                        doc.menu_at = Some((l, col));
                        doc.kb_menu = true;
                    }
                }
            }
            if doc.kb_menu {
                let mut open = true;
                egui::Window::new("Line actions").collapsible(false).resizable(false).open(&mut open).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ui.ctx(), |ui| {
                    if let Some(a) = line_menu(ui, doc, &text, st_now) {
                        act = Some(a);
                        doc.kb_menu = false;
                    }
                });
                if !open || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    doc.kb_menu = false;
                }
            }
            }
            let Some(_) = doc.text.clone() else {
                ui.spinner();
                if let Some(v) = switch {
                    let o = doc.open.clone();
                    self.open(o, Some(v), None);
                }
                return;
            };
            if let Some(v) = switch {
                let o = doc.open.clone();
                self.open(o, Some(v), None);
            }
            if let Some(name) = usages {
                self.usages_of(&name);
            }
            if let Some(a) = act {
                self.act(a, ui.ctx());
            }
            if let Some(t) = toast_msg {
                self.toast(t);
            }
            if let Some((name, new)) = change {
                let what = if new.is_some() { "replaced" } else { "deleted" };
                self.edits.insert(name.clone(), new);
                let what = if name == "AndroidManifest.xml" && what == "replaced" { "updated" } else { what };
                self.toast(format!("{name} will be {what} when you build (Files tab)"));
            }
            if let Some(src) = stage_smali {
                self.smali_edits.push(src);
                self.toast(format!("Smali staged ({} class(es)) — use Build & sign to write it", self.smali_edits.len()));
            }
        });
        self.export_window(ctx);
        self.note_window(ctx);
        self.edit_windows(ctx);
        self.settings_window(ctx);
        if let Some((t, at)) = &self.toast {
            if at.elapsed().as_secs() < 4 {
                egui::Area::new("toast".into())
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -36.0])
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(p.panel)
                            .stroke(Stroke::new(1.0_f32, p.accent))
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.label(t);
                            });
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            } else {
                self.toast = None;
            }
        }
        let indexing = self.st.is_some_and(|s| s.index.get().is_none());
        let busy = (self.st.is_none() && !self.path.is_empty() && self.err.is_none())
            || self.prog.is_some()
            || indexing
            || self.docs.iter().any(|d| d.text.is_none());
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

pub fn run(path: Option<&str>, deobf: bool) -> Result<(), String> {
    // opengl32 is delay-loaded (see build.rs) and the GL loader finds it with GetModuleHandle
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut u8;
        }
        unsafe { LoadLibraryA(c"opengl32.dll".as_ptr().cast()) };
    }
    let icon = egui::IconData {
        rgba: include_bytes!("../assets/icon64.rgba").to_vec(),
        width: 64,
        height: 64,
    };
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 700.0])
        .with_maximized(true)
        .with_title("apkscope")
        .with_icon(Arc::new(icon))
        .with_drag_and_drop(true);
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let path = path.map(str::to_string);
    eframe::run_native(
        "apkscope",
        opts,
        Box::new(move |_| Ok(Box::new(App::new(path, deobf)))),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_splits_tokens_and_marks_line() {
        let p = Theme::Dark.palette();
        let job = highlight(
            "int x = 1; // hi\nString s = \"a\\\"b\";",
            &p,
            13.0,
            Some(2),
            None,
        );
        let parts: Vec<&str> = job
            .sections
            .iter()
            .map(|s| &job.text[s.byte_range.clone()])
            .collect();
        assert!(
            parts.contains(&"int")
                && parts.contains(&"// hi")
                && parts.contains(&"\"a\\\"b\"")
                && parts.contains(&"1"),
            "{parts:?}"
        );
        // only line 2 gets a background
        let marked: String = job
            .sections
            .iter()
            .filter(|s| s.format.background != Color32::TRANSPARENT)
            .map(|s| &job.text[s.byte_range.clone()])
            .collect();
        assert_eq!(marked, "String s = \"a\\\"b\";");
    }

    #[test]
    fn tree_nests_packages() {
        let names = ["a.b.C".to_string(), "a.D".to_string(), "E".to_string()];
        let t = tree(names.iter());
        assert_eq!(t.leaves, vec![("E".to_string(), "E".to_string())]);
        assert_eq!(t.dirs["a"].leaves[0].0, "D");
        assert_eq!(t.dirs["a"].dirs["b"].leaves[0].1, "a.b.C");
    }

    #[test]
    fn finds_member_under_cursor() {
        let java = "package a;\n\npublic class Outer {\n    private int count;\n\n    /* WARNING: 1 jump could not be structured. */\n    public static String a(int i) {\n        int x = i;\n        return null;\n    }\n\n    static class In {\n        void run() {\n        }\n    }\n}\n";
        assert_eq!(member_at(java, 2), Some((vec!["Outer".into()], None)));
        assert_eq!(
            member_at(java, 3),
            Some((vec!["Outer".into()], Some((false, "count".into()))))
        );
        // the WARNING comment points at the method below it
        assert_eq!(
            member_at(java, 5),
            Some((vec!["Outer".into()], Some((true, "a".into()))))
        );
        assert_eq!(member_at(java, 7), None);
        assert_eq!(
            member_at(java, 12),
            Some((
                vec!["Outer".into(), "In".into()],
                Some((true, "run".into()))
            ))
        );
    }

    #[test]
    fn words_declarations_and_call_lines() {
        let java = "public class A {
    void run(int x) {
        B b = new B();
        b.go(x);
        helper();
    }

    void helper() {
        go();
    }
}
";
        assert_eq!(word_at(java, 2, 18), Some(("B".into(), true, true, None)));
        assert_eq!(
            word_at(java, 3, 11),
            Some(("go".into(), true, false, Some("b".into())))
        );
        assert_eq!(word_at(java, 3, 8), Some(("b".into(), false, false, None)));
        assert_eq!(find_decl(java, "helper", Some(0)), Some(8));
        assert_eq!(find_decl(java, "run", Some(1)), Some(2));
        // the call inside `run`, not the declaration and not the `go()` in helper
        let t = Target {
            decl: Some(("run".into(), Some(1))),
            then: Some("go(".into()),
        };
        assert_eq!(locate(java, &t), Some(4));
        let t = Target {
            decl: Some(("helper".into(), None)),
            then: Some("go(".into()),
        };
        assert_eq!(locate(java, &t), Some(9));
        assert_eq!(classes_around(java, 3), vec!["A".to_string()]);
    }

    #[test]
    fn short_names_and_find() {
        assert_eq!(Open::Class("a.b.Foo".into()).short(), "Foo");
        assert_eq!(Open::Res("res/layout/main.xml".into()).short(), "main.xml");
        assert_eq!(Open::Overview.short(), "Overview");
        let t = "a\nfoo\nb\nFOO";
        assert_eq!(find_next(t, "foo", 0), Some(2));
        assert_eq!(find_next(t, "foo", 2), Some(4));
        // wraps around
        assert_eq!(find_next(t, "foo", 4), Some(2));
        assert_eq!(find_next(t, "zzz", 0), None);
    }
}
