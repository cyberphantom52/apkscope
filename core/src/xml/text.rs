//! A [`Document`] as text, the way `res::decode_xml` writes it: one element per line,
//! attributes one per line under it, namespaces declared where they start.

use super::*;
use crate::res::{Names, format_value, framework, framework_constant, xml_escape};
use std::collections::HashMap;
use std::fmt::Write;

const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";

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
