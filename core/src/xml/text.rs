//! A [`Document`] as text, the way `res::decode_xml` writes it: one element per line,
//! attributes one per line under it, namespaces declared where they start.

use super::*;
use crate::markup::{Element, Node as TextNode, parse_xml};
use crate::res::{
    Names, format_value, framework, framework_constant, framework_constant_value, xml_escape,
};
use crate::values::{Index, typed};
use crate::{Error, Result};
use std::collections::HashMap;
use std::fmt::Write;

const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
const RES_AUTO: &str = "http://schemas.android.com/apk/res-auto";
const RES_PACKAGE: &str = "http://schemas.android.com/apk/res/";

impl Document {
    pub(crate) fn string(&self, i: u32) -> &str {
        self.strings
            .strings
            .get(i as usize)
            .map_or("", String::as_str)
    }

    /// An attribute's name as text, `android:layout_width`, with the namespace prefixes in
    /// scope (URI -> prefix).
    pub(crate) fn attribute_name(
        &self,
        a: &Attribute,
        ns: &HashMap<String, String>,
        names: &Names,
    ) -> String {
        let id = self.ids.get(a.name as usize).copied();
        // Android reads framework attributes by ID alone; packers blank or fake their name and
        // namespace strings, so the ID decides both
        if let Some(("attr", fw)) = id.and_then(framework) {
            let prefix = ns.get(ANDROID_NS).map_or("android", String::as_str);
            return format!("{prefix}:{fw}");
        }
        let mut name = self.string(a.name).to_owned();
        if name.is_empty() {
            // an obfuscated attribute name: what its ID names
            name = id.map_or(format!("attr{}", a.name), |id| {
                names
                    .name(id)
                    .map_or(format!("attr_{id:08x}"), str::to_owned)
            });
        }
        match (a.ns != NONE).then(|| ns.get(self.string(a.ns))).flatten() {
            Some(prefix) => format!("{prefix}:{name}"),
            None => name,
        }
    }

    /// An attribute's value as text: the string it was written as, a framework constant
    /// (`match_parent`, `center`), or its typed value.
    pub(crate) fn attribute_value(&self, a: &Attribute, name: &str, names: &Names) -> String {
        if a.raw != NONE {
            return self.string(a.raw).to_owned();
        }
        let local = name.rsplit(':').next().unwrap_or(name);
        framework_constant(local, a.value)
            .unwrap_or_else(|| format_value(a.value, &self.strings.strings, names))
    }
}

/// The document as text.
pub fn decode(doc: &Document, names: &Names) -> String {
    let mut ns: HashMap<String, String> = HashMap::new(); // URI -> prefix
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    let mut depth = 0usize;
    let mut open_tag = false; // the last start tag not yet closed with `>`
    for n in &doc.nodes {
        match n {
            Node::StartNamespace(n) => {
                let (prefix, uri) = (
                    doc.string(n.prefix).to_owned(),
                    doc.string(n.uri).to_owned(),
                );
                ns.insert(uri.clone(), prefix.clone());
                pending.push((prefix, uri));
            }
            Node::Start(e) => {
                if open_tag {
                    out += ">\n";
                }
                let pad = "    ".repeat(depth);
                let _ = write!(out, "{pad}<{}", doc.string(e.name));
                let attr_pad = format!("\n{pad}    ");
                for (prefix, uri) in pending.drain(..) {
                    let _ = write!(out, "{attr_pad}xmlns:{prefix}=\"{}\"", xml_escape(&uri));
                }
                for a in &e.attrs {
                    let name = doc.attribute_name(a, &ns, names);
                    let value = doc.attribute_value(a, &name, names);
                    let _ = write!(out, "{attr_pad}{name}=\"{}\"", xml_escape(&value));
                }
                open_tag = true;
                depth += 1;
            }
            Node::End(e) => {
                depth = depth.saturating_sub(1);
                if open_tag {
                    out += " />\n";
                    open_tag = false;
                } else {
                    let _ = writeln!(out, "{}</{}>", "    ".repeat(depth), doc.string(e.name));
                }
            }
            Node::Text(t) => {
                if open_tag {
                    out += ">\n";
                    open_tag = false;
                }
                let _ = writeln!(
                    out,
                    "{}{}",
                    "    ".repeat(depth),
                    xml_escape(doc.string(t.data).trim())
                );
            }
            Node::EndNamespace(_) | Node::Other(_) => {}
        }
    }
    out
}

// ---------- text back into a document ----------

/// An element of the original document: its start and end nodes, its attributes' names as
/// [`decode`] writes them, and its children. The namespaces declared with it are the nodes
/// just before its start and just after its end.
struct Original {
    start: usize,
    end: usize,
    names: Vec<String>,
    children: Vec<Child>,
}

enum Child {
    Element(Original),
    Text(usize),
}

/// The document's root element as a tree, its attribute names as `decode` writes them: the
/// prefix of a namespace is the latest one declared for it anywhere before.
fn tree(doc: &Document, names: &Names) -> Option<Original> {
    let mut ns: HashMap<String, String> = HashMap::new();
    let mut stack: Vec<Original> = vec![];
    for (i, n) in doc.nodes.iter().enumerate() {
        match n {
            Node::StartNamespace(n) => {
                ns.insert(
                    doc.string(n.uri).to_owned(),
                    doc.string(n.prefix).to_owned(),
                );
            }
            Node::Start(e) => stack.push(Original {
                start: i,
                end: i,
                names: e
                    .attrs
                    .iter()
                    .map(|a| doc.attribute_name(a, &ns, names))
                    .collect(),
                children: vec![],
            }),
            Node::End(_) => {
                let Some(mut e) = stack.pop() else { continue };
                e.end = i;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Child::Element(e)),
                    None => return Some(e),
                }
            }
            Node::Text(_) => {
                if let Some(e) = stack.last_mut() {
                    e.children.push(Child::Text(i));
                }
            }
            Node::EndNamespace(_) | Node::Other(_) => {}
        }
    }
    None
}

/// The namespace nodes just before `start` and just after `end`.
fn namespaces(doc: &Document, e: &Original) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let mut first = e.start;
    while first > 0 && matches!(doc.nodes[first - 1], Node::StartNamespace(_)) {
        first -= 1;
    }
    let mut last = e.end + 1;
    while matches!(doc.nodes.get(last), Some(Node::EndNamespace(_))) {
        last += 1;
    }
    (first..e.start, e.end + 1..last)
}

/// What `apply` builds: the new document, and the names of new attributes that need a
/// resource ID, which go into the pool's first strings once everything is written.
struct Builder<'a> {
    doc: &'a Document,
    out: Document,
    names: &'a Names,
    index: Index,
    /// Names with an ID to add to the start of the pool: placeholder `PENDING + i`.
    named: Vec<(String, u32)>,
}

/// A string reference waiting for its place among the named strings.
const PENDING: u32 = 0xffff_0000;

impl Builder<'_> {
    fn string(&mut self, s: &str) -> u32 {
        self.out.strings.intern(s)
    }

    /// A string that names an attribute with resource ID `id`.
    fn named(&mut self, s: &str, id: u32) -> u32 {
        let found = (0..self.out.ids.len())
            .find(|&i| self.out.ids[i] == id && self.out.strings.strings[i] == s);
        if let Some(i) = found {
            return i as u32;
        }
        let k = match self.named.iter().position(|n| n.0 == s && n.1 == id) {
            Some(k) => k,
            None => {
                self.named.push((s.to_owned(), id));
                self.named.len() - 1
            }
        };
        PENDING + k as u32
    }

    /// Put the new named strings after the pool's first ones, moving every later string.
    fn finish(mut self) -> Document {
        if self.named.is_empty() {
            return self.out;
        }
        let at = self.out.ids.len() as u32;
        let added = self.named.len() as u32;
        for (k, (s, id)) in self.named.iter().enumerate() {
            self.out.strings.strings.insert(at as usize + k, s.clone());
            self.out.ids.push(*id);
        }
        let shift = |r: &mut u32| {
            if *r >= PENDING && *r != NONE {
                *r = at + (*r - PENDING);
            } else if *r != NONE && *r >= at {
                *r += added;
            }
        };
        let value = |v: &mut Value| {
            if v.ty == Value::STRING {
                shift(&mut v.data);
            }
        };
        for n in &mut self.out.nodes {
            match n {
                Node::StartNamespace(n) | Node::EndNamespace(n) => {
                    shift(&mut n.source.comment);
                    shift(&mut n.prefix);
                    shift(&mut n.uri);
                }
                Node::Start(e) => {
                    shift(&mut e.source.comment);
                    shift(&mut e.ns);
                    shift(&mut e.name);
                    for a in &mut e.attrs {
                        shift(&mut a.ns);
                        shift(&mut a.name);
                        shift(&mut a.raw);
                        value(&mut a.value);
                    }
                }
                Node::End(e) => {
                    shift(&mut e.source.comment);
                    shift(&mut e.ns);
                    shift(&mut e.name);
                }
                Node::Text(t) => {
                    shift(&mut t.source.comment);
                    shift(&mut t.data);
                    value(&mut t.value);
                }
                Node::Other(_) => {}
            }
        }
        self.out
    }

    /// A value for attribute `name` as text: a framework constant, or a typed value; a string
    /// is kept as written too.
    fn value(&mut self, name: &str, text: &str, string: bool) -> Result<(u32, Value)> {
        let local = name.rsplit(':').next().unwrap_or(name);
        if !string && let Some(v) = framework_constant_value(local, text) {
            return Ok((NONE, v));
        }
        let mut raw = NONE;
        let v = if string {
            let i = self.string(text);
            Value {
                ty: Value::STRING,
                data: i,
            }
        } else {
            let mut stored = |pool: &mut crate::table::StringPool| -> Result<u32> {
                let i = pool.intern(text);
                raw = i;
                Ok(i)
            };
            typed(text, "", &self.index, &mut self.out.strings, &mut stored)?
        };
        if v.ty == Value::STRING {
            raw = v.data;
        }
        Ok((raw, v))
    }

    fn element(
        &mut self,
        orig: Option<&Original>,
        e: &Element,
        scope: &[(String, String)],
    ) -> Result<()> {
        let orig = orig.filter(|o| {
            matches!(&self.doc.nodes[o.start], Node::Start(s) if self.doc.string(s.name) == e.name)
        });
        // namespaces: `xmlns:` attributes
        let declared: Vec<(String, String)> = e
            .attrs
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix("xmlns:")?.to_owned(), v.clone())))
            .collect();
        let (starts, ends) = match orig {
            Some(o) => namespaces(self.doc, o),
            None => (0..0, 0..0),
        };
        let same_ns = starts.len() == declared.len()
            && self.doc.nodes[starts.clone()].iter().zip(&declared).all(|(n, (p, u))| {
                matches!(n, Node::StartNamespace(n) if self.doc.string(n.prefix) == p && self.doc.string(n.uri) == u)
            });
        let source = Source {
            line: e.line as u32,
            comment: NONE,
        };
        let mut ns_nodes = vec![];
        if same_ns {
            self.out
                .nodes
                .extend(self.doc.nodes[starts].iter().cloned());
        } else {
            for (p, u) in &declared {
                let n = Namespace {
                    source,
                    prefix: self.string(p),
                    uri: self.string(u),
                };
                ns_nodes.push(n.clone());
                self.out.nodes.push(Node::StartNamespace(n));
            }
        }
        let mut scope = scope.to_vec();
        scope.extend(declared);
        // attributes, matched with the original's by name
        let original: Option<&Start> = orig.and_then(|o| match &self.doc.nodes[o.start] {
            Node::Start(s) => Some(s),
            _ => None,
        });
        let mut used = vec![false; original.map_or(0, |s| s.attrs.len())];
        let mut attrs = vec![];
        let mut changed = original.is_none();
        let given: Vec<&(String, String)> = e
            .attrs
            .iter()
            .filter(|(k, _)| !k.starts_with("xmlns:"))
            .collect();
        for (k, v) in &given {
            let found = original.and_then(|s| {
                let o = orig.expect("with its start");
                (0..s.attrs.len()).find(|&i| !used[i] && o.names[i] == *k)
            });
            match (found, original) {
                (Some(i), Some(s)) => {
                    used[i] = true;
                    let a = &s.attrs[i];
                    if self.doc.attribute_value(a, k, self.names) == *v {
                        attrs.push((a.clone(), self.doc.ids.get(a.name as usize).copied()));
                    } else {
                        changed = true;
                        let (raw, value) = self.value(k, v, a.raw != NONE)?;
                        attrs.push((
                            Attribute {
                                raw,
                                value,
                                ..a.clone()
                            },
                            self.doc.ids.get(a.name as usize).copied(),
                        ));
                    }
                }
                _ => {
                    changed = true;
                    let (prefix, local) = match k.split_once(':') {
                        Some((p, l)) => (Some(p), l),
                        None => (None, k.as_str()),
                    };
                    let uri = match prefix {
                        Some(p) => match scope.iter().rev().find(|(sp, _)| sp == p) {
                            Some((_, u)) => Some(u.clone()),
                            // packers drop the declaration; Android reads its attributes by ID
                            None if p == "android" => Some(ANDROID_NS.to_owned()),
                            None => {
                                return Err(Error::xml(format!(
                                    "line {}: undeclared prefix in {k}",
                                    e.line
                                )));
                            }
                        },
                        None => None,
                    };
                    // only Android's and the app's own namespaces name attributes with IDs
                    let id = match uri.as_deref() {
                        Some(ANDROID_NS) => Some(
                            self.index
                                .resolve(&format!("android:attr/{local}"), "attr")?,
                        ),
                        Some(u) if u == RES_AUTO || u.starts_with(RES_PACKAGE) => {
                            self.index.resolve(&format!("attr/{local}"), "attr").ok()
                        }
                        _ => None,
                    };
                    let name = match id {
                        Some(id) => self.named(local, id),
                        None => self.string(local),
                    };
                    let ns = uri.map_or(NONE, |u| self.string(&u));
                    let (raw, value) = self.value(k, v, false)?;
                    attrs.push((
                        Attribute {
                            ns,
                            name,
                            raw,
                            value,
                        },
                        id,
                    ));
                }
            }
        }
        if used.iter().any(|u| !u) {
            changed = true;
        }
        let start = match (changed, original) {
            (false, Some(s)) => s.clone(),
            _ => {
                // aapt2's order: attributes with a resource ID by ID, then the others as written
                attrs.sort_by_key(|(_, id)| id.map_or(u64::MAX, u64::from));
                let index_of = |want: &dyn Fn(&str, &str) -> bool, this: &Self| {
                    attrs
                        .iter()
                        .position(|(a, _)| {
                            let local = this
                                .out
                                .strings
                                .strings
                                .get(a.name as usize)
                                .map_or("", String::as_str);
                            let ns = if a.ns == NONE {
                                ""
                            } else {
                                this.out
                                    .strings
                                    .strings
                                    .get(a.ns as usize)
                                    .map_or("", String::as_str)
                            };
                            want(ns, local)
                        })
                        .map_or(0, |i| i as u16 + 1)
                };
                let id_index = index_of(
                    &|ns, n| n == "id" && (ns.is_empty() || ns == ANDROID_NS),
                    self,
                );
                let class_index = index_of(&|ns, n| n == "class" && ns.is_empty(), self);
                let style_index = index_of(&|ns, n| n == "style" && ns.is_empty(), self);
                Start {
                    source: original.map_or(source, |s| s.source),
                    ns: original.map_or(NONE, |s| s.ns),
                    name: match original {
                        Some(s) => s.name,
                        None => self.string(&e.name),
                    },
                    id_index,
                    class_index,
                    style_index,
                    attrs: attrs.into_iter().map(|(a, _)| a).collect(),
                }
            }
        };
        let (ns, name) = (start.ns, start.name);
        self.out.nodes.push(Node::Start(start));
        // children, matched with the original's in order
        self.children(orig, e, &scope)?;
        match orig {
            Some(o) => {
                self.out.nodes.push(self.doc.nodes[o.end].clone());
                if same_ns {
                    self.out.nodes.extend(self.doc.nodes[ends].iter().cloned());
                }
            }
            None => self.out.nodes.push(Node::End(End { source, ns, name })),
        }
        if !same_ns {
            for n in ns_nodes.into_iter().rev() {
                self.out.nodes.push(Node::EndNamespace(n));
            }
        }
        Ok(())
    }

    fn children(
        &mut self,
        orig: Option<&Original>,
        e: &Element,
        scope: &[(String, String)],
    ) -> Result<()> {
        // the text's children, as `decode` writes them: whitespace between elements is layout
        let given: Vec<&TextNode> = e
            .children
            .iter()
            .filter(|n| !matches!(n, TextNode::Text(t) if t.trim().is_empty()))
            .collect();
        let had: &[Child] = orig.map_or(&[], |o| &o.children);
        // what pairs them: an element's name, or text
        let given_keys: Vec<String> = given
            .iter()
            .map(|n| match n {
                TextNode::Element(e) => e.name.clone(),
                TextNode::Text(_) => String::new(),
            })
            .collect();
        let had_keys: Vec<String> = had
            .iter()
            .map(|c| match c {
                Child::Element(o) => match &self.doc.nodes[o.start] {
                    Node::Start(s) => self.doc.string(s.name).to_owned(),
                    _ => String::new(),
                },
                Child::Text(_) => String::new(),
            })
            .collect();
        // the longest common sequence of names pairs them; the rest are new or gone
        let (n, m) = (given.len(), had.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if given_keys[i] == had_keys[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n {
            let pair = j < m && given_keys[i] == had_keys[j] && lcs[i][j] == lcs[i + 1][j + 1] + 1;
            let skip_had = j < m && !pair && lcs[i][j + 1] >= lcs[i + 1][j];
            if skip_had {
                j += 1;
                continue;
            }
            let o = pair.then(|| &had[j]);
            match given[i] {
                TextNode::Element(child) => {
                    let o = o.and_then(|c| match c {
                        Child::Element(o) => Some(o),
                        Child::Text(_) => None,
                    });
                    self.element(o, child, scope)?;
                }
                TextNode::Text(t) => {
                    let t = t.trim();
                    match o {
                        Some(Child::Text(k)) if matches!(&self.doc.nodes[*k], Node::Text(x) if self.doc.string(x.data).trim() == t) =>
                        {
                            self.out.nodes.push(self.doc.nodes[*k].clone());
                        }
                        _ => {
                            let data = self.string(t);
                            self.out.nodes.push(Node::Text(Text {
                                source: Source {
                                    line: e.line as u32,
                                    comment: NONE,
                                },
                                data,
                                value: Value { ty: 0, data: 0 },
                                value_size: 0,
                            }));
                        }
                    }
                }
            }
            i += 1;
            if pair {
                j += 1;
            }
        }
        Ok(())
    }
}

/// The document text says, as [`decode`] writes it, with `doc` as the original: whatever the
/// text leaves as it was is kept exactly as it is in `doc`, so an unchanged text gives `doc`
/// back.
pub fn apply(doc: &Document, text: &str, names: &Names) -> Result<Document> {
    let root = parse_xml(text)?;
    let original = tree(doc, names);
    let mut b = Builder {
        doc,
        out: Document {
            strings: doc.strings.clone(),
            ids: doc.ids.clone(),
            nodes: vec![],
        },
        names,
        index: Index::from_names(names),
        named: vec![],
    };
    b.element(original.as_ref(), &root, &[])?;
    Ok(b.finish())
}
