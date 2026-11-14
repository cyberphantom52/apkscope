//! Binary XML bytes -> [`Document`].

use super::*;
use crate::table::read::{chunk, pool, u16_at, u32_at};
use crate::{Error, Result};

const RES_XML: u16 = 0x0003;
pub(crate) const RES_STRING_POOL: u16 = 0x0001;
pub(crate) const RES_XML_RESOURCE_MAP: u16 = 0x0180;
pub(crate) const START_NAMESPACE: u16 = 0x0100;
pub(crate) const END_NAMESPACE: u16 = 0x0101;
pub(crate) const START_ELEMENT: u16 = 0x0102;
pub(crate) const END_ELEMENT: u16 = 0x0103;
pub(crate) const CDATA: u16 = 0x0104;

pub fn read(d: &[u8]) -> Result<Document> {
    let (ty, header, size) = chunk(d, 0)?;
    if ty != RES_XML {
        return Err(Error::resources("not binary XML"));
    }
    let mut strings = None;
    let mut ids = Vec::new();
    let mut nodes = Vec::new();
    let mut at = header;
    while at < size {
        let (ty, header, len) = chunk(d, at)?;
        let body = at + header;
        let source = || -> Result<Source> {
            Ok(Source {
                line: u32_at(d, at + 8)?,
                comment: u32_at(d, at + 12)?,
            })
        };
        let value = |o: usize| -> Result<Value> {
            Ok(Value {
                ty: *d
                    .get(o + 3)
                    .ok_or_else(|| Error::resources("value past end"))?,
                data: u32_at(d, o + 4)?,
            })
        };
        match ty {
            RES_STRING_POOL => strings = Some(pool(d, at)?),
            RES_XML_RESOURCE_MAP => {
                ids = (0..(len - header) / 4)
                    .map(|k| u32_at(d, body + k * 4))
                    .collect::<Result<_>>()?
            }
            START_NAMESPACE | END_NAMESPACE => {
                let ns = Namespace {
                    source: source()?,
                    prefix: u32_at(d, body)?,
                    uri: u32_at(d, body + 4)?,
                };
                nodes.push(match ty {
                    START_NAMESPACE => Node::StartNamespace(ns),
                    _ => Node::EndNamespace(ns),
                });
            }
            START_ELEMENT => {
                let attr_start = u16_at(d, body + 8)? as usize;
                let attr_size = u16_at(d, body + 10)? as usize;
                let count = u16_at(d, body + 12)? as usize;
                let attrs = (0..count)
                    .map(|k| {
                        let a = body + attr_start + k * attr_size;
                        Ok(Attribute {
                            ns: u32_at(d, a)?,
                            name: u32_at(d, a + 4)?,
                            raw: u32_at(d, a + 8)?,
                            value: value(a + 12)?,
                        })
                    })
                    .collect::<Result<_>>()?;
                nodes.push(Node::Start(Start {
                    source: source()?,
                    ns: u32_at(d, body)?,
                    name: u32_at(d, body + 4)?,
                    id_index: u16_at(d, body + 14)?,
                    class_index: u16_at(d, body + 16)?,
                    style_index: u16_at(d, body + 18)?,
                    attrs,
                }));
            }
            END_ELEMENT => nodes.push(Node::End(End {
                source: source()?,
                ns: u32_at(d, body)?,
                name: u32_at(d, body + 4)?,
            })),
            CDATA => nodes.push(Node::Text(Text {
                source: source()?,
                data: u32_at(d, body)?,
                value: value(body + 4)?,
                value_size: u16_at(d, body + 4)?,
            })),
            _ => nodes.push(Node::Other(d[at..at + len].to_vec())),
        }
        at += len;
    }
    Ok(Document {
        strings: strings.ok_or_else(|| Error::resources("binary XML without a string pool"))?,
        ids,
        nodes,
    })
}
