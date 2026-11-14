//! `res/values*/*.xml` -> [`Table`]: the inverse of [`crate::res::decode_table`], so decoded
//! resources can be edited as text and put back. Strings follow aapt2's rules: escapes,
//! quotes that keep whitespace, whitespace that collapses otherwise, and markup that becomes
//! style spans.

use crate::markup::{Element, Node, parse_xml};
use crate::res::{self, Names};
use crate::table::{Entry, EntryValue, Package, Span, StringPool, Table, Value, config};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};

// ---------- strings, as aapt2 reads them ----------

/// A resource string: its text and its spans (tag with attributes, first and last character,
/// in UTF-16 units).
#[derive(Debug, PartialEq)]
struct Styled {
    text: String,
    spans: Vec<(String, u32, u32)>,
}

/// The string an element's body stands for: escapes resolved, whitespace collapsed outside
/// quotes and trimmed, child elements as spans.
fn styled(e: &Element) -> Result<Styled> {
    struct Builder {
        out: String,
        units: u32,
        quoted: bool,
        /// whether the last character is a space that collapsing produced
        soft_space: bool,
        spans: Vec<(String, u32, u32)>,
    }
    impl Builder {
        fn push(&mut self, c: char, soft: bool) {
            self.out.push(c);
            self.units += c.len_utf16() as u32;
            self.soft_space = soft;
        }
        fn text(&mut self, t: &str, line: usize) -> Result<()> {
            let mut chars = t.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        let e = chars.next().ok_or_else(|| {
                            Error::xml(format!("line {line}: `\\` at the end of a string"))
                        })?;
                        let c = match e {
                            'n' => '\n',
                            't' => '\t',
                            'u' => {
                                let hex: String = chars.by_ref().take(4).collect();
                                u32::from_str_radix(&hex, 16)
                                    .ok()
                                    .and_then(char::from_u32)
                                    .ok_or_else(|| {
                                        Error::xml(format!("line {line}: bad \\u{hex}"))
                                    })?
                            }
                            other => other,
                        };
                        self.push(c, false);
                    }
                    '"' => self.quoted = !self.quoted,
                    c if c.is_whitespace() && !self.quoted => {
                        if !self.out.is_empty() && !self.soft_space {
                            self.push(' ', true);
                        }
                    }
                    c => self.push(c, false),
                }
            }
            Ok(())
        }
        fn node(&mut self, n: &Node) -> Result<()> {
            match n {
                Node::Text(t) => self.text(t, 0),
                Node::Element(e) => {
                    let mut tag = e.name.clone();
                    for (k, v) in &e.attrs {
                        tag.push(';');
                        tag.push_str(k);
                        tag.push('=');
                        tag.push_str(v);
                    }
                    let first = self.units;
                    let at = self.spans.len();
                    self.spans.push((tag, first, 0));
                    for c in &e.children {
                        self.node(c)?;
                    }
                    self.spans[at].2 = self.units.wrapping_sub(1);
                    Ok(())
                }
            }
        }
    }
    let mut b = Builder {
        out: String::new(),
        units: 0,
        quoted: false,
        soft_space: false,
        spans: vec![],
    };
    for c in &e.children {
        b.node(c)?;
    }
    if b.soft_space {
        b.out.pop();
        b.units -= 1;
        for s in &mut b.spans {
            s.1 = s.1.min(b.units);
            if s.2 != u32::MAX && s.2 >= b.units {
                s.2 = b.units.wrapping_sub(1);
            }
        }
    }
    Ok(Styled {
        text: b.out,
        spans: b.spans,
    })
}

impl StringPool {
    /// The index of a string with exactly these spans, added if the pool has none.
    fn intern_styled(&mut self, s: &Styled) -> u32 {
        if s.spans.is_empty() {
            // a plain string: one with no style entry, or an empty one
            if let Some(i) = (0..self.strings.len()).find(|&i| {
                self.strings[i] == s.text && self.styles.get(i).is_none_or(Vec::is_empty)
            }) {
                return i as u32;
            }
            self.strings.push(s.text.clone());
            self.sorted = false;
            return (self.strings.len() - 1) as u32;
        }
        let spans: Vec<Span> = s
            .spans
            .iter()
            .map(|(name, first, last)| Span {
                name: self.intern(name),
                first: *first,
                last: *last,
            })
            .collect();
        if let Some(i) =
            (0..self.styles.len()).find(|&i| self.strings[i] == s.text && self.styles[i] == spans)
        {
            return i as u32;
        }
        self.strings.push(s.text.clone());
        self.sorted = false;
        let i = self.strings.len() - 1;
        // a styled string's spans are at its index; the strings before it get none
        self.styles.resize(i, vec![]);
        self.styles.push(spans);
        i as u32
    }

    fn styled_at(&self, i: u32) -> Option<Styled> {
        let text = self.strings.get(i as usize)?.clone();
        let spans = self
            .styles
            .get(i as usize)
            .map(|spans| {
                spans
                    .iter()
                    .map(|s| {
                        (
                            self.strings
                                .get(s.name as usize)
                                .cloned()
                                .unwrap_or_default(),
                            s.first,
                            s.last,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Styled { text, spans })
    }
}

// ---------- typed values ----------

/// Resources by type and name, in the app package and the framework.
pub(crate) struct Index {
    app: u32,
    ids: HashMap<(String, String), u32>,
}

impl Index {
    /// What `names` names, the package that names resources without a package being `app`.
    pub(crate) fn from_names(names: &Names) -> Index {
        // a name two packages give, as framework-res's staged ones do, is the lowest ID's: the
        // public one
        let mut ids: HashMap<(String, String), u32> = HashMap::new();
        for (id, t, n) in names.iter() {
            let at = ids.entry((t.to_owned(), n.to_owned())).or_insert(id);
            *at = (*at).min(id);
        }
        Index {
            app: names.app,
            ids,
        }
    }

    fn new(p: &Package) -> Index {
        let ids = res::resource_names(p)
            .into_iter()
            .map(|(id, name)| (name, id))
            .collect();
        Index { app: p.id, ids }
    }

    /// `type/name`, `android:type/name`, or `0x7f010000`.
    pub(crate) fn resolve(&self, body: &str, default_type: &str) -> Result<u32> {
        let body = body.strip_prefix('+').unwrap_or(body);
        let (android, body) = match body.strip_prefix("android:") {
            Some(b) => (true, b),
            None => (false, body),
        };
        if let Some(hex) = body.strip_prefix("0x") {
            return u32::from_str_radix(hex, 16)
                .map_err(|_| Error::xml(format!("bad resource ID 0x{hex}")));
        }
        let (t, n) = body.split_once('/').unwrap_or((default_type, body));
        // one with no public name is written as its ID: `android:attr/0x01120069`
        if let Some(hex) = n.strip_prefix("0x") {
            return u32::from_str_radix(hex, 16)
                .map_err(|_| Error::xml(format!("bad resource ID 0x{hex}")));
        }
        let found = if android && self.app != 0x01 {
            res::framework_id(t, n)
        } else {
            self.ids.get(&(t.to_owned(), n.to_owned())).copied()
        };
        found.ok_or_else(|| {
            let android = if android { "android:" } else { "" };
            Error::EntryNotFound(format!("{android}{t}/{n}"))
        })
    }
}

/// A value written as text, of the kind its element or type says: `@string/x`, `?attr/x`,
/// `#ff0000`, `16dp`, `50%`, `true`, `12`, `0x1f`, `1.5`, or else a string.
pub(crate) fn typed(
    text: &str,
    kind: &str,
    index: &Index,
    pool: &mut StringPool,
    string: &mut dyn FnMut(&mut StringPool) -> Result<u32>,
) -> Result<Value> {
    let v = |ty, data| Ok(Value { ty, data });
    let t = text.trim();
    match t {
        "@null" => return v(Value::NULL, 0),
        "@empty" => return v(Value::NULL, 1),
        _ => {}
    }
    // a reference, in any element: a string that starts with `@` or `?` escapes it
    if let Some(r) = t.strip_prefix('@') {
        return v(Value::REFERENCE, index.resolve(r, kind)?);
    }
    if let Some(r) = t.strip_prefix('?') {
        return v(Value::ATTRIBUTE, index.resolve(r, "attr")?);
    }
    if kind != "string" {
        match t {
            "true" => return v(Value::INT_BOOLEAN, u32::MAX),
            "false" => return v(Value::INT_BOOLEAN, 0),
            _ => {}
        }
        if let Some(hex) = t.strip_prefix('#')
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(n) = u32::from_str_radix(hex, 16)
        {
            let ty = match hex.len() {
                8 => Value::INT_COLOR_ARGB8,
                6 => Value::INT_COLOR_RGB8,
                4 => Value::INT_COLOR_ARGB4,
                3 => Value::INT_COLOR_RGB4,
                _ => 0,
            };
            if ty != 0 {
                return v(ty, n);
            }
        }
        if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
            && let Ok(n) = u32::from_str_radix(hex, 16)
        {
            return v(Value::INT_HEX, n);
        }
        if let Ok(n) = t.parse::<i64>()
            && (i32::MIN as i64..=u32::MAX as i64).contains(&n)
            && kind != "dimen"
            && kind != "fraction"
        {
            return v(Value::INT_DEC, n as u32);
        }
        let units = |units: &[&str], scale: f32| {
            // the longest unit that ends it: `%p` before `%`, `dip` as `dp`
            let mut found = None;
            for (code, unit) in units.iter().enumerate() {
                let alias = if *unit == "dp" { Some("dip") } else { None };
                for u in std::iter::once(*unit).chain(alias) {
                    if let Some(num) = t.strip_suffix(u)
                        && let Ok(f) = num.trim().parse::<f32>()
                        && found.is_none_or(|(_, l, _)| u.len() > l)
                    {
                        found = Some((code as u32, u.len(), f));
                    }
                }
            }
            found.map(|(code, _, f)| res::float_to_complex(f * scale, code))
        };
        if let Some(c) = units(&res::DIMENSION_UNITS, 1.0) {
            return v(Value::DIMENSION, c);
        }
        if let Some(c) = units(&res::FRACTION_UNITS, 0.01) {
            return v(Value::FRACTION, c);
        }
        if let Ok(f) = t.parse::<f32>() {
            return v(Value::FLOAT, f.to_bits());
        }
        if matches!(kind, "bool" | "integer" | "color" | "dimen" | "fraction") {
            return Err(Error::xml(format!("`{t}` is not a {kind}")));
        }
    }
    v(Value::STRING, string(pool)?)
}

// ---------- elements ----------

/// What a values file says one resource is.
struct Resource<'a> {
    ty: String,
    name: String,
    element: &'a Element,
}

/// The resources a file defines, by type and name.
fn resources(root: &Element) -> Result<Vec<Resource<'_>>> {
    let mut out = vec![];
    for e in root.elements() {
        let name = || {
            e.attr("name")
                .map(str::to_owned)
                .ok_or_else(|| Error::xml(format!("line {}: <{}> without a name", e.line, e.name)))
        };
        let ty = match e.name.as_str() {
            "string" | "bool" | "integer" | "color" | "dimen" | "fraction" | "style" | "attr"
            | "plurals" => e.name.clone(),
            "string-array" | "integer-array" | "array" => "array".into(),
            "drawable" => "drawable".into(),
            "item" => e
                .attr("type")
                .map(str::to_owned)
                .ok_or_else(|| Error::xml(format!("line {}: <item> without a type", e.line)))?,
            // what aapt2 compiles into R and not into the table, and public.xml
            "declare-styleable" | "eat-comment" | "skip" | "public" | "public-group"
            | "java-symbol" | "add-resource" | "overlayable" => continue,
            other => {
                return Err(Error::xml(format!(
                    "line {}: unknown resource element <{other}>",
                    e.line
                )));
            }
        };
        out.push(Resource {
            ty,
            name: name()?,
            element: e,
        });
    }
    Ok(out)
}

const PLURALS: [(&str, u32); 6] = [
    ("other", 0x0100_0004),
    ("zero", 0x0100_0005),
    ("one", 0x0100_0006),
    ("two", 0x0100_0007),
    ("few", 0x0100_0008),
    ("many", 0x0100_0009),
];

const ATTR_FORMATS: [(&str, u32); 10] = [
    ("reference", 0x01),
    ("string", 0x02),
    ("integer", 0x04),
    ("boolean", 0x08),
    ("color", 0x10),
    ("float", 0x20),
    ("dimension", 0x40),
    ("fraction", 0x80),
    ("enum", 0x1_0000),
    ("flags", 0x2_0000),
];

/// A resource's value, from its element. Where the text says what `orig` already is, `orig`
/// is kept as it is: text does not hold every bit of a value, so rewriting an unchanged one
/// could change it.
fn value(
    r: &Resource,
    orig: Option<&EntryValue>,
    index: &Index,
    names: &Names,
    pool: &mut StringPool,
    keep: bool,
) -> Result<EntryValue> {
    let e = r.element;
    let simple = |text: &str,
                  kind: &str,
                  orig: Option<Value>,
                  pool: &mut StringPool,
                  e: &Element|
     -> Result<Value> {
        if keep && let Some(o) = orig {
            let same = match o.ty {
                Value::STRING => pool
                    .styled_at(o.data)
                    .is_some_and(|s| styled(e).is_ok_and(|n| n == s)),
                _ => res::format_value(o, &pool.strings, names) == text.trim(),
            };
            if same {
                return Ok(o);
            }
        }
        typed(text, kind, index, pool, &mut |pool| {
            Ok(pool.intern_styled(&styled(e)?))
        })
    };
    let orig_simple = match orig {
        Some(EntryValue::Simple(v)) => Some(*v),
        _ => None,
    };
    let orig_map: HashMap<u32, Value> = match orig {
        Some(EntryValue::Complex { map, .. }) => map.iter().copied().collect(),
        _ => HashMap::new(),
    };
    let complex = |parent, map| Ok(EntryValue::Complex { parent, map });
    match e.name.as_str() {
        "style" => {
            let parent = match e.attr("parent") {
                Some("") => 0,
                Some(p) => {
                    let p = p.strip_prefix('@').unwrap_or(p);
                    let p = p.strip_prefix("style/").unwrap_or(p);
                    let p = p.replace("android:style/", "android:");
                    let (android, p) = match p.strip_prefix("android:") {
                        Some(b) => (true, b.to_owned()),
                        None => (false, p),
                    };
                    index.resolve(
                        &format!("{}style/{p}", if android { "android:" } else { "" }),
                        "style",
                    )?
                }
                // `A.B` is under `A`, if there is one
                None => r
                    .name
                    .rsplit_once('.')
                    .and_then(|(p, _)| index.resolve(&format!("style/{p}"), "style").ok())
                    .unwrap_or(0),
            };
            let mut map = vec![];
            for item in e.elements() {
                let attr = item.attr("name").ok_or_else(|| {
                    Error::xml(format!("line {}: style <item> without a name", item.line))
                })?;
                let key = match attr.strip_prefix("android:") {
                    Some(a) => index.resolve(&format!("android:attr/{a}"), "attr")?,
                    // `colorPrimary`, or a type of its own: `^attr-private/x`
                    None if attr.contains('/') => index.resolve(attr, "attr")?,
                    None => index.resolve(&format!("attr/{attr}"), "attr")?,
                };
                map.push((
                    key,
                    simple(&item.text(), "", orig_map.get(&key).copied(), pool, item)?,
                ));
            }
            complex(parent, map)
        }
        "plurals" => {
            let mut map = vec![];
            for item in e.elements() {
                let q = item.attr("quantity").unwrap_or("other");
                let key = PLURALS
                    .iter()
                    .find(|p| p.0 == q)
                    .ok_or_else(|| {
                        Error::xml(format!("line {}: unknown quantity `{q}`", item.line))
                    })?
                    .1;
                map.push((
                    key,
                    simple(
                        &item.text(),
                        "string",
                        orig_map.get(&key).copied(),
                        pool,
                        item,
                    )?,
                ));
            }
            complex(0, map)
        }
        "string-array" | "integer-array" | "array" => {
            let kind = match e.name.as_str() {
                "string-array" => "string",
                "integer-array" => "integer",
                _ => "",
            };
            let mut map = vec![];
            for (i, item) in e.elements().enumerate() {
                let key = 0x0100_0001 + i as u32;
                map.push((
                    key,
                    simple(&item.text(), kind, orig_map.get(&key).copied(), pool, item)?,
                ));
            }
            complex(0, map)
        }
        "attr" => {
            let mut format: u32 = e
                .attr("format")
                .map(|f| {
                    f.split('|')
                        .filter_map(|f| ATTR_FORMATS.iter().find(|x| x.0 == f.trim()))
                        .map(|x| x.1)
                        .fold(0, |a, b| a | b)
                })
                .unwrap_or(0);
            // every format is how "any" decodes
            if format == 0xff {
                format = 0xffff;
            }
            let mut kids = vec![];
            for k in e.elements() {
                format |= match k.name.as_str() {
                    "enum" => 0x1_0000,
                    "flag" => 0x2_0000,
                    _ => 0,
                };
                let n = k.attr("name").unwrap_or_default();
                let id = index.resolve(&format!("id/{n}"), "id")?;
                let val = k.attr("value").unwrap_or("0");
                let data = match val.strip_prefix("0x") {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => val.parse::<i64>().ok().map(|n| n as u32),
                }
                .ok_or_else(|| Error::xml(format!("line {}: bad value `{val}`", k.line)))?;
                let ty = if val.starts_with("0x") {
                    Value::INT_HEX
                } else {
                    Value::INT_DEC
                };
                kids.push((
                    id,
                    orig_map
                        .get(&id)
                        .copied()
                        .filter(|o| o.data == data)
                        .unwrap_or(Value { ty, data }),
                ));
            }
            // what the text does not say (min, max, localization) stays as it was
            let mut map: Vec<(u32, Value)> = vec![];
            let meta = |k: u32| orig_map.get(&k).copied();
            let format_value = match meta(0x0100_0000) {
                Some(o) if e.attr("format").is_none() && !kids.is_empty() => o,
                Some(o) if o.data & 0xffff == format & 0xffff || format == 0 => Value {
                    ty: o.ty,
                    data: if format == 0 {
                        o.data
                    } else {
                        format | (o.data & !0x3_ffff)
                    },
                },
                _ => Value {
                    ty: Value::INT_DEC,
                    data: if format == 0 { 0xffff } else { format },
                },
            };
            map.push((0x0100_0000, format_value));
            for k in [0x0100_0001, 0x0100_0002, 0x0100_0003] {
                if let Some(o) = meta(k) {
                    map.push((k, o));
                }
            }
            map.extend(kids);
            if let Some(EntryValue::Complex { map: om, .. }) = orig {
                // the original's order, when it has the same keys
                let order: Vec<u32> = om.iter().map(|x| x.0).collect();
                let mut keys: Vec<u32> = map.iter().map(|x| x.0).collect();
                keys.sort();
                let mut sorted = order.clone();
                sorted.sort();
                if keys == sorted {
                    map.sort_by_key(|(k, _)| order.iter().position(|o| o == k));
                }
            }
            complex(0, map)
        }
        _ if !e.elements().any(|_| true) || matches!(e.name.as_str(), "string") => {
            let kind = match e.name.as_str() {
                "item" => "",
                k => k,
            };
            if r.ty == "id" {
                return Ok(EntryValue::Simple(orig_simple.unwrap_or(Value {
                    ty: Value::INT_BOOLEAN,
                    data: 0,
                })));
            }
            Ok(EntryValue::Simple(simple(
                &e.text(),
                kind,
                orig_simple,
                pool,
                e,
            )?))
        }
        _ => {
            // any other bag: `<item type="T" name="n"><item name="@id/k">v</item></item>`
            let mut map = vec![];
            for item in e.elements() {
                let k = item.attr("name").unwrap_or_default();
                let key = index.resolve(k.trim_start_matches('@'), "attr")?;
                map.push((
                    key,
                    simple(&item.text(), "", orig_map.get(&key).copied(), pool, item)?,
                ));
            }
            complex(0, map)
        }
    }
}

// ---------- applying files ----------

/// The type a values file holds by default, as [`crate::res::decode_table`] names them:
/// `strings.xml` holds strings.
fn file_types(file: &str) -> Vec<String> {
    match file {
        "plurals" => vec!["plurals".into()],
        f => vec![f.strip_suffix('s').unwrap_or(f).to_owned()],
    }
}

/// Apply `res/values*/*.xml` files, by their path under `res/` (`values-night/colors.xml`), to
/// the app package. Each file says everything its configuration has of the types in it: a
/// value it has is set or added, one it no longer has is removed. `values/public.xml` gives
/// new resources their IDs.
pub fn apply(t: &mut Table, files: &BTreeMap<String, String>) -> Result<()> {
    apply_with(t, files, true)
}

fn apply_with(t: &mut Table, files: &BTreeMap<String, String>, keep: bool) -> Result<()> {
    let names = Names::new(t);
    let Some(app) = t.app().map(|p| p.id) else {
        return match files.is_empty() {
            true => Ok(()),
            false => Err(Error::resources("no package to put values in")),
        };
    };
    let mut public: HashMap<(String, String), u32> = HashMap::new();
    if let Some(text) = files.get("values/public.xml") {
        for p in parse_xml(text)?.elements().filter(|e| e.name == "public") {
            if let (Some(ty), Some(name), Some(id)) = (p.attr("type"), p.attr("name"), p.attr("id"))
            {
                let id = u32::from_str_radix(id.trim_start_matches("0x"), 16)
                    .map_err(|_| Error::xml(format!("line {}: bad id `{id}`", p.line)))?;
                public.insert((ty.to_owned(), name.to_owned()), id);
            }
        }
    }
    for (path, text) in files {
        let (dir, file) = path
            .split_once('/')
            .ok_or_else(|| Error::resources(format!("{path}: not under a values directory")))?;
        let Some(qualifiers) = dir.strip_prefix("values") else {
            continue;
        };
        if file == "public.xml" {
            continue;
        }
        let at = config::parse(qualifiers.trim_start_matches('-')).map_err(|e| e.at(path))?;
        let root = parse_xml(text).map_err(|e| e.at(path))?;
        let defined = resources(&root).map_err(|e| e.at(path))?;
        // the types this file has a say over
        let mut types: BTreeSet<String> = file_types(file.trim_end_matches(".xml"))
            .into_iter()
            .collect();
        types.extend(defined.iter().map(|r| r.ty.clone()));
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        let index = Index::new(t.package(app).expect("the app package"));
        for r in &defined {
            let p = t.package(app).expect("the app package");
            let id = index.ids.get(&(r.ty.clone(), r.name.clone())).copied();
            let orig = id
                .and_then(|id| p.get(id, &at))
                .map(|e: &Entry| e.value.clone());
            let orig = orig
                .or_else(|| id.and_then(|id| p.values(id).next().map(|(_, e)| e.value.clone())));
            let v = value(r, orig.as_ref(), &index, &names, &mut t.strings, keep)
                .map_err(|e| e.at(path))?;
            let p = t.package_mut(app).expect("the app package");
            let id = match id {
                Some(id) => {
                    if p.get(id, &at).map(|e| &e.value) != Some(&v) {
                        p.set(id, &at, v)?;
                    }
                    id
                }
                None => match public.get(&(r.ty.clone(), r.name.clone())) {
                    Some(&id) => {
                        p.add_as(id, &r.ty, &r.name, &at, v)?;
                        id
                    }
                    None => p.add(&r.ty, &r.name, &at, v)?,
                },
            };
            seen.insert(id);
        }
        // what the file no longer has; a file resource (a string naming a path under res/) is
        // the file's, not a values file's
        let p = t.package(app).expect("the app package");
        let file = |v: &EntryValue| match v {
            EntryValue::Simple(Value {
                ty: Value::STRING,
                data,
            }) => t
                .strings
                .strings
                .get(*data as usize)
                .is_some_and(|s| s.starts_with("res/")),
            _ => false,
        };
        let gone: Vec<u32> = index
            .ids
            .iter()
            .filter(|((ty, _), id)| {
                types.contains(ty)
                    && !seen.contains(id)
                    && p.get(**id, &at).is_some_and(|e| !file(&e.value))
            })
            .map(|(_, id)| *id)
            .collect();
        let p = t.package_mut(app).expect("the app package");
        for id in gone {
            p.remove(id, &at);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{self, Chunk};

    /// The first entries in which two tables of the same shape differ.
    fn differences(a: &Table, b: &Table) -> Vec<String> {
        let mut out = vec![];
        if a.strings != b.strings {
            out.push(format!(
                "global pool: {} vs {} strings",
                a.strings.strings.len(),
                b.strings.strings.len()
            ));
        }
        for (p, q) in a.packages.iter().zip(&b.packages) {
            if p.keys != q.keys {
                out.push(format!(
                    "keys: {} vs {}",
                    p.keys.strings.len(),
                    q.keys.strings.len()
                ));
            }
            if p.chunks.len() != q.chunks.len() {
                out.push(format!("chunks: {} vs {}", p.chunks.len(), q.chunks.len()));
            }
            for (c, d) in p.chunks.iter().zip(&q.chunks) {
                match (c, d) {
                    (Chunk::Type(x), Chunk::Type(y)) if x != y => {
                        for (i, (e, f)) in x.entries.iter().zip(&y.entries).enumerate() {
                            if e != f && out.len() < 8 {
                                let id = p.id << 24 | (x.id as u32) << 16 | i as u32;
                                out.push(format!(
                                    "{:?} in -{}: {e:?} vs {f:?}",
                                    p.name(id),
                                    config::name(&x.config)
                                ));
                            }
                        }
                    }
                    (Chunk::Spec(x), Chunk::Spec(y)) if x != y => {
                        out.push(format!("spec {}", x.id))
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Decoding every table in a directory and applying what it decodes to gives the table back
    /// unchanged:
    /// APKSCOPE_TABLES=<dir> cargo test --release values_round_trip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn values_round_trip() {
        let dir = std::env::var("APKSCOPE_TABLES").expect("set APKSCOPE_TABLES");
        let (mut same, mut failed) = (0, 0);
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let Ok(t) = table::read(&std::fs::read(&path).unwrap()) else {
                continue;
            };
            let names = Names::new(&t);
            let decoded = res::decode_table(&t, &names);
            let mut back = t.clone();
            match apply(&mut back, &decoded.values) {
                Err(e) => {
                    failed += 1;
                    println!("{}: {}", path.display(), e.report());
                }
                Ok(()) if back == t => same += 1,
                Ok(()) => {
                    failed += 1;
                    println!(
                        "{}:\n  {}",
                        path.display(),
                        differences(&t, &back).join("\n  ")
                    );
                }
            }
        }
        println!("{same} tables unchanged, {failed} not");
        assert_eq!(failed, 0);
    }
}

#[cfg(test)]
mod edits {
    use super::*;
    use crate::table::{Chunk, Spec, Type};

    fn pool(strings: &[&str]) -> StringPool {
        StringPool {
            utf8: true,
            sorted: false,
            strings: strings.iter().map(|s| s.to_string()).collect(),
            styles: vec![],
        }
    }

    /// One package with one string, `hello`.
    fn sample() -> Table {
        Table {
            strings: pool(&["Hello"]),
            packages: vec![Package {
                id: 0x7f,
                name: "com.example".into(),
                last_public_type: 0,
                last_public_key: 0,
                type_id_offset: 0,
                types: pool(&["string"]),
                keys: pool(&["hello"]),
                chunks: vec![
                    Chunk::Spec(Spec {
                        id: 1,
                        res0: 0,
                        res1: 1,
                        flags: vec![0],
                    }),
                    Chunk::Type(Type {
                        id: 1,
                        flags: 0,
                        reserved: 0,
                        config: config::default(),
                        entries: vec![Some(Entry {
                            flags: 0,
                            key: 0,
                            value: EntryValue::Simple(Value {
                                ty: Value::STRING,
                                data: 0,
                            }),
                        })],
                    }),
                ],
            }],
        }
    }

    fn files(f: &[(&str, &str)]) -> BTreeMap<String, String> {
        f.iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn edits_through_values_files() {
        let mut t = sample();
        let strings = r#"<resources>
    <string name="hello">Tap <b>here</b>, now</string>
    <string name="quoted">"  two  spaces " and \"quotes\"</string>
    <string name="pinned">@string/hello</string>
</resources>"#;
        apply(
            &mut t,
            &files(&[
                ("values/strings.xml", strings),
                ("values-night/bools.xml", r#"<resources><bool name="dark">true</bool></resources>"#),
                ("values/integers.xml", r#"<resources><integer name="answer">42</integer></resources>"#),
                ("values/public.xml", r#"<resources><public type="string" name="pinned" id="0x7f010005" /></resources>"#),
            ]),
        )
        .unwrap();
        let p = t.app().unwrap();
        let hello = p.find("string", "hello").unwrap();
        let EntryValue::Simple(v) = &p.get(hello, &config::default()).unwrap().value else {
            panic!("a simple value")
        };
        assert_eq!(t.strings.strings[v.data as usize], "Tap here, now");
        let spans = &t.strings.styles[v.data as usize];
        assert_eq!((spans[0].first, spans[0].last), (4, 7));
        assert_eq!(t.strings.strings[spans[0].name as usize], "b");
        assert_eq!(p.find("string", "pinned"), Some(0x7f01_0005));
        assert_eq!(p.find("string", "quoted"), Some(0x7f01_0001));
        let night = config::parse("night").unwrap();
        assert!(p.get(p.find("bool", "dark").unwrap(), &night).is_some());

        // what it decodes to says the same
        let decoded = res::decode_table(&t, &Names::new(&t)).values;
        let s = &decoded["values/strings.xml"];
        assert!(
            s.contains(r#"<string name="hello">Tap <b>here</b>, now</string>"#),
            "{s}"
        );
        assert!(
            s.contains(r#"<string name="quoted">"  two  spaces  and \"quotes\""</string>"#),
            "{s}"
        );
        assert!(
            s.contains(r#"<string name="pinned">@string/hello</string>"#),
            "{s}"
        );
        assert!(
            decoded["values/public.xml"]
                .contains(r#"<public type="string" name="pinned" id="0x7f010005" />"#)
        );
        assert!(decoded["values/integers.xml"].contains(r#"<integer name="answer">42</integer>"#));

        // a string the file no longer has is removed; its ID stays taken
        let fewer = strings.replace(
            r#"    <string name="quoted">"  two  spaces " and \"quotes\"</string>"#,
            "",
        );
        apply(&mut t, &files(&[("values/strings.xml", &fewer)])).unwrap();
        let p = t.app().unwrap();
        assert_eq!(p.find("string", "quoted"), None);
        assert!(p.find("string", "hello").is_some());
        let back = crate::table::read(&crate::table::write(&t)).unwrap();
        assert_eq!(back, t);
    }
}
