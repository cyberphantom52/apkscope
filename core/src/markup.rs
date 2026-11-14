//! A small XML reader that keeps text as written, for the text forms of resources: values
//! files, where whitespace and markup mean something, and binary XML's text form.

use crate::{Error, Result};

#[derive(Debug)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug)]
pub(crate) struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub line: usize,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// The text of its children, markup removed.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for n in &self.children {
            match n {
                Node::Text(t) => s.push_str(t),
                Node::Element(e) => s.push_str(&e.text()),
            }
        }
        s
    }
}

fn entities(s: &str, line: usize) -> Result<String> {
    if !s.contains('&') {
        return Ok(s.to_owned());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let end = rest[i..]
            .find(';')
            .ok_or_else(|| Error::xml(format!("line {line}: `&` without `;`")))?;
        let name = &rest[i + 1..i + end];
        let c = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => name
                .strip_prefix("#x")
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| name.strip_prefix('#').map(str::parse))
                .and_then(|n| n.ok())
                .and_then(char::from_u32)
                .ok_or_else(|| Error::xml(format!("line {line}: unknown entity &{name};")))?,
        };
        out.push(c);
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub(crate) fn parse_xml(text: &str) -> Result<Element> {
    let mut stack: Vec<Element> = vec![Element {
        name: String::new(),
        attrs: vec![],
        children: vec![],
        line: 0,
    }];
    let mut i = 0;
    let line_at = |i: usize| text[..i].matches('\n').count() + 1;
    let unterminated =
        |what: &str, i: usize| Error::xml(format!("line {}: unterminated {what}", line_at(i)));
    while i < text.len() {
        let rest = &text[i..];
        if let Some(r) = rest.strip_prefix("<!--") {
            i += 4 + r.find("-->").ok_or_else(|| unterminated("comment", i))? + 3;
        } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or_else(|| unterminated("CDATA", i))?;
            let top = stack.last_mut().expect("the document");
            top.children.push(Node::Text(r[..end].to_owned()));
            i += 9 + end + 3;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            i += rest
                .find('>')
                .ok_or_else(|| unterminated("declaration", i))?
                + 1;
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or_else(|| unterminated("end tag", i))?;
            let name = r[..end].trim();
            let e = stack.pop().filter(|e| e.name == name && !e.name.is_empty());
            let e =
                e.ok_or_else(|| Error::xml(format!("line {}: unexpected </{name}>", line_at(i))))?;
            stack
                .last_mut()
                .expect("the document")
                .children
                .push(Node::Element(e));
            i += 2 + end + 1;
        } else if rest.starts_with('<') {
            let line = line_at(i);
            let mut j = 1;
            let b = rest.as_bytes();
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let name = rest[1..j].to_owned();
            let mut attrs = vec![];
            loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                match b.get(j) {
                    None => return Err(unterminated(&format!("<{name}>"), i)),
                    Some(b'>') => {
                        stack.push(Element {
                            name,
                            attrs,
                            children: vec![],
                            line,
                        });
                        j += 1;
                        break;
                    }
                    Some(b'/') => {
                        let e = Element {
                            name,
                            attrs,
                            children: vec![],
                            line,
                        };
                        stack
                            .last_mut()
                            .expect("the document")
                            .children
                            .push(Node::Element(e));
                        j += 2;
                        break;
                    }
                    _ => {
                        let eq = rest[j..]
                            .find('=')
                            .ok_or_else(|| unterminated("attribute", i))?;
                        let key = rest[j..j + eq].trim().to_owned();
                        let q = j
                            + eq
                            + 1
                            + rest[j + eq + 1..]
                                .find(['"', '\''])
                                .ok_or_else(|| unterminated("attribute", i))?;
                        let quote = b[q] as char;
                        let end = q
                            + 1
                            + rest[q + 1..]
                                .find(quote)
                                .ok_or_else(|| unterminated("attribute", i))?;
                        attrs.push((key, entities(&rest[q + 1..end], line)?));
                        j = end + 1;
                    }
                }
            }
            i += j;
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let t = entities(&rest[..end], line_at(i))?;
            stack
                .last_mut()
                .expect("the document")
                .children
                .push(Node::Text(t));
            i += end;
        }
    }
    let mut doc = stack
        .pop()
        .filter(|_| stack.is_empty())
        .ok_or_else(|| Error::xml("an element is never closed"))?;
    let root = doc.children.drain(..).find_map(|n| match n {
        Node::Element(e) => Some(e),
        Node::Text(_) => None,
    });
    root.ok_or_else(|| Error::xml("no root element"))
}
