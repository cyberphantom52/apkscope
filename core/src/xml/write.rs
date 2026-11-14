//! [`Document`] -> binary XML bytes, laid out as aapt2 lays them out.

use super::read::*;
use super::*;
use crate::table::write::{chunk, pool, u16, u32};

fn value(o: &mut Vec<u8>, v: Value) {
    sized(o, v, 8);
}

fn sized(o: &mut Vec<u8>, v: Value, size: u16) {
    u16(o, size);
    o.push(0);
    o.push(v.ty);
    u32(o, v.data);
}

fn node(o: &mut Vec<u8>, ty: u16, source: Source, body: impl FnOnce(&mut Vec<u8>)) {
    chunk(o, ty, 16, |o| {
        u32(o, source.line);
        u32(o, source.comment);
        body(o);
    });
}

pub fn write(doc: &Document) -> Vec<u8> {
    let mut o = Vec::new();
    chunk(&mut o, 0x0003, 8, |o| {
        pool(o, &doc.strings);
        chunk(o, RES_XML_RESOURCE_MAP, 8, |o| {
            doc.ids.iter().for_each(|&id| u32(o, id))
        });
        for n in &doc.nodes {
            match n {
                Node::StartNamespace(ns) | Node::EndNamespace(ns) => {
                    let ty = match n {
                        Node::StartNamespace(_) => START_NAMESPACE,
                        _ => END_NAMESPACE,
                    };
                    node(o, ty, ns.source, |o| {
                        u32(o, ns.prefix);
                        u32(o, ns.uri);
                    });
                }
                Node::Start(e) => node(o, START_ELEMENT, e.source, |o| {
                    u32(o, e.ns);
                    u32(o, e.name);
                    u16(o, 20);
                    u16(o, 20);
                    u16(o, e.attrs.len() as u16);
                    u16(o, e.id_index);
                    u16(o, e.class_index);
                    u16(o, e.style_index);
                    for a in &e.attrs {
                        u32(o, a.ns);
                        u32(o, a.name);
                        u32(o, a.raw);
                        value(o, a.value);
                    }
                }),
                Node::End(e) => node(o, END_ELEMENT, e.source, |o| {
                    u32(o, e.ns);
                    u32(o, e.name);
                }),
                Node::Text(t) => node(o, CDATA, t.source, |o| {
                    u32(o, t.data);
                    sized(o, t.value, t.value_size);
                }),
                Node::Other(raw) => o.extend_from_slice(raw),
            }
        }
    });
    o
}
