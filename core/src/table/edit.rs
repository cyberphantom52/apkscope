//! Changing a [`Table`]: looking resources up by name, and setting, adding and removing their
//! values per configuration. Resource IDs never move, since code refers to them; a new entry
//! gets the next free ID in its type.

use super::*;
use crate::{Error, Result};

/// A spec flag that is not a configuration bit: public, staged API.
const SPEC_NOT_CONFIG: u32 = 0xf000_0000;

impl StringPool {
    pub fn index(&self, s: &str) -> Option<u32> {
        self.strings.iter().position(|x| x == s).map(|i| i as u32)
    }

    /// The index of `s`, added at the end if the pool does not have it.
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(i) = self.index(s) {
            return i;
        }
        self.strings.push(s.to_owned());
        self.sorted = false;
        (self.strings.len() - 1) as u32
    }
}

impl Table {
    pub fn package(&self, id: u32) -> Option<&Package> {
        self.packages.iter().find(|p| p.id == id)
    }

    pub fn package_mut(&mut self, id: u32) -> Option<&mut Package> {
        self.packages.iter_mut().find(|p| p.id == id)
    }

    /// The app's own package, `0x7f`, or the first if there is none: some apps put a library
    /// package first.
    pub fn app(&self) -> Option<&Package> {
        self.package(0x7f).or(self.packages.first())
    }

    pub fn app_mut(&mut self) -> Option<&mut Package> {
        let id = self.app()?.id;
        self.package_mut(id)
    }
}

/// `0xPPTTEEEE`: package, type and entry index.
fn split(id: u32) -> (u32, u8, usize) {
    (id >> 24, (id >> 16) as u8, (id & 0xffff) as usize)
}

impl Package {
    /// A type's ID, `attr`, `string`, ...
    pub fn type_id(&self, name: &str) -> Option<u8> {
        let i = self.types.index(name)?;
        Some((i + 1 + self.type_id_offset) as u8)
    }

    pub fn type_name(&self, id: u8) -> Option<&str> {
        let i = (id as u32).checked_sub(1 + self.type_id_offset)?;
        self.types.strings.get(i as usize).map(String::as_str)
    }

    fn resource(&self, ty: u8, index: usize) -> u32 {
        self.id << 24 | (ty as u32) << 16 | index as u32
    }

    fn spec(&self, ty: u8) -> Option<&Spec> {
        self.chunks.iter().find_map(|c| match c {
            Chunk::Spec(s) if s.id == ty => Some(s),
            _ => None,
        })
    }

    fn types_of(&self, ty: u8) -> impl Iterator<Item = &Type> {
        self.chunks.iter().filter_map(move |c| match c {
            Chunk::Type(t) if t.id == ty => Some(t),
            _ => None,
        })
    }

    /// The configuration a resource directory's qualifiers (`xxhdpi`, `en-rIN`) name here: one
    /// the package has exactly, or else a new one as aapt2 would compile it for an app whose
    /// `minSdkVersion` is `min_sdk` ([`config::versioned`]).
    pub fn config(&self, qualifiers: &str, min_sdk: u16) -> Result<Vec<u8>> {
        let c = config::parse(qualifiers)?;
        let has = self
            .chunks
            .iter()
            .any(|ch| matches!(ch, Chunk::Type(t) if t.config == c));
        Ok(if has {
            c
        } else {
            config::versioned(&c, min_sdk)
        })
    }

    /// The ID of resource `name` of type `ty`.
    pub fn find(&self, ty: &str, name: &str) -> Option<u32> {
        let (ty, key) = (self.type_id(ty)?, self.keys.index(name)?);
        self.types_of(ty).find_map(|t| {
            t.entries
                .iter()
                .position(|e| e.as_ref().is_some_and(|e| e.key == key))
                .map(|i| self.resource(ty, i))
        })
    }

    /// A resource's type and name.
    pub fn name(&self, id: u32) -> Option<(&str, &str)> {
        let (_, ty, _) = split(id);
        let key = self.values(id).next()?.1.key;
        Some((self.type_name(ty)?, self.keys.strings.get(key as usize)?))
    }

    /// A resource's value in `config`, if it has one there.
    pub fn get(&self, id: u32, config: &[u8]) -> Option<&Entry> {
        let (_, ty, index) = split(id);
        self.types_of(ty)
            .find(|t| t.config == config)?
            .entries
            .get(index)?
            .as_ref()
    }

    /// A resource's values, with the configuration of each.
    pub fn values(&self, id: u32) -> impl Iterator<Item = (&[u8], &Entry)> {
        let (_, ty, index) = split(id);
        self.types_of(ty)
            .filter_map(move |t| Some((t.config.as_slice(), t.entries.get(index)?.as_ref()?)))
    }

    /// Set a resource's value in `config`, which it may not have had a value in yet.
    pub fn set(&mut self, id: u32, config: &[u8], value: EntryValue) -> Result<()> {
        let (_, ty, index) = split(id);
        let known = self.values(id).next().map(|(_, e)| (e.key, e.flags));
        let Some((key, flags)) = known else {
            return Err(Error::EntryNotFound(format!("resource {id:#010x}")));
        };
        self.put(ty, index, config, key, flags & Entry::PUBLIC, value);
        Ok(())
    }

    /// Add resource `name` of type `ty` with `value` in `config`, and return its ID. If the
    /// package has it already, this sets its value there instead.
    pub fn add(&mut self, ty: &str, name: &str, config: &[u8], value: EntryValue) -> Result<u32> {
        if let Some(id) = self.find(ty, name) {
            self.set(id, config, value)?;
            return Ok(id);
        }
        let type_id = match self.type_id(ty) {
            Some(id) if self.spec(id).is_some() => id,
            _ => self.add_type(ty)?,
        };
        let key = self.keys.intern(name);
        let spec = self.spec_mut(type_id);
        let index = spec.flags.len();
        if index > 0xffff {
            return Err(Error::resources(format!(
                "type {ty} has no room for {name}"
            )));
        }
        spec.flags.push(0);
        self.put(type_id, index, config, key, 0, value);
        Ok(self.resource(type_id, index))
    }

    /// Add resource `name` of type `ty` at `id`, which no other resource has: for IDs that
    /// were given, as `public.xml` gives them.
    pub fn add_as(
        &mut self,
        id: u32,
        ty: &str,
        name: &str,
        config: &[u8],
        value: EntryValue,
    ) -> Result<()> {
        let (package, type_id, index) = split(id);
        let bad = |why: &str| Error::resources(format!("{ty}/{name} can't be {id:#010x}: {why}"));
        if package != self.id {
            return Err(bad("another package"));
        }
        match self.type_id(ty) {
            Some(t) if t == type_id && self.spec(t).is_some() => {}
            Some(t) if t == type_id => {
                self.add_type(ty)?;
            }
            None if self.types.strings.len() as u32 + 1 + self.type_id_offset == type_id as u32 => {
                self.add_type(ty)?;
            }
            _ => return Err(bad("another type")),
        }
        if self.values(id).next().is_some() {
            return Err(bad("taken"));
        }
        let key = self.keys.intern(name);
        let spec = self.spec_mut(type_id);
        if spec.flags.len() <= index {
            spec.flags.resize(index + 1, 0);
        }
        self.put(type_id, index, config, key, 0, value);
        Ok(())
    }

    /// Remove a resource's value in `config`; its ID stays taken.
    pub fn remove(&mut self, id: u32, config: &[u8]) -> bool {
        let (_, ty, index) = split(id);
        let removed = self.chunks.iter_mut().any(|c| match c {
            Chunk::Type(t) if t.id == ty && t.config == config => {
                t.entries.get_mut(index).and_then(Option::take).is_some()
            }
            _ => false,
        });
        if removed {
            self.refresh_spec(ty, index);
            // a configuration with no values left has no type chunk: Android's loader, and
            // aapt2, don't take an empty one
            let before = self.chunks.len();
            self.chunks.retain(|c| {
                !matches!(c, Chunk::Type(t) if t.id == ty && t.config == config
                    && t.entries.iter().all(Option::is_none))
            });
            if self.chunks.len() != before {
                let types = self.types_of(ty).count() as u16;
                let spec = self.spec_mut(ty);
                if spec.res1 != 0 {
                    spec.res1 = types;
                }
            }
        }
        removed
    }

    fn spec_mut(&mut self, ty: u8) -> &mut Spec {
        self.chunks
            .iter_mut()
            .find_map(|c| match c {
                Chunk::Spec(s) if s.id == ty => Some(s),
                _ => None,
            })
            .expect("a type with a spec")
    }

    /// A new type, its ID the next after the package's last.
    fn add_type(&mut self, name: &str) -> Result<u8> {
        let i = self.types.intern(name);
        let id = u8::try_from(i + 1 + self.type_id_offset)
            .map_err(|_| Error::resources(format!("no room for type {name}")))?;
        let at = self
            .chunks
            .iter()
            .rposition(|c| matches!(c, Chunk::Spec(_) | Chunk::Type(_)))
            .map_or(0, |i| i + 1);
        self.chunks.insert(
            at,
            Chunk::Spec(Spec {
                id,
                res0: 0,
                res1: 0,
                flags: vec![],
            }),
        );
        Ok(id)
    }

    /// Store an entry, in the type chunk for `config`, which is added if there is none.
    fn put(
        &mut self,
        ty: u8,
        index: usize,
        config: &[u8],
        key: u32,
        public: u16,
        value: EntryValue,
    ) {
        let count = self.spec(ty).map_or(0, |s| s.flags.len());
        let at = match self
            .chunks
            .iter()
            .position(|c| matches!(c, Chunk::Type(t) if t.id == ty && t.config == config))
        {
            Some(at) => at,
            None => {
                let last = self
                    .chunks
                    .iter()
                    .rposition(|c| {
                        matches!(c, Chunk::Spec(s) if s.id == ty)
                            || matches!(c, Chunk::Type(t) if t.id == ty)
                    })
                    .expect("a type with a spec");
                self.chunks.insert(
                    last + 1,
                    Chunk::Type(Type {
                        id: ty,
                        flags: 0,
                        reserved: 0,
                        config: config.to_vec(),
                        entries: vec![None; count],
                    }),
                );
                let types = self.types_of(ty).count() as u16;
                let spec = self.spec_mut(ty);
                // since Android 13, res1 counts the type's chunks
                if spec.res1 != 0 {
                    spec.res1 = types;
                }
                last + 1
            }
        };
        let Chunk::Type(t) = &mut self.chunks[at] else {
            unreachable!("found above")
        };
        if t.entries.len() <= index {
            t.entries.resize(index + 1, None);
        }
        let old = t.entries[index].take();
        let flags = old.map_or(public, |e| e.flags & !(Entry::COMPLEX | Entry::COMPACT));
        let complex = matches!(value, EntryValue::Complex { .. });
        t.entries[index] = Some(Entry {
            flags: flags | if complex { Entry::COMPLEX } else { 0 },
            key,
            value,
        });
        self.refresh_spec(ty, index);
    }

    /// An entry's spec flags: the configuration bits in which its values differ, as aapt2
    /// computes them (from every pair of them), and its public and staged flags as they were.
    fn refresh_spec(&mut self, ty: u8, index: usize) {
        let configs: Vec<Vec<u8>> = self
            .types_of(ty)
            .filter(|t| t.entries.get(index).is_some_and(Option::is_some))
            .map(|t| t.config.clone())
            .collect();
        // a field differs between two of them exactly when one differs from the first
        let bits = configs.split_first().map_or(0, |(first, rest)| {
            rest.iter().fold(0, |b, c| b | config::diff(first, c))
        });
        let flag = &mut self.spec_mut(ty).flags[index];
        *flag = *flag & SPEC_NOT_CONFIG | bits;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(strings: &[&str]) -> StringPool {
        StringPool {
            utf8: true,
            sorted: false,
            strings: strings.iter().map(|s| s.to_string()).collect(),
            styles: vec![],
        }
    }

    fn string(i: u32) -> EntryValue {
        EntryValue::Simple(Value {
            ty: Value::STRING,
            data: i,
        })
    }

    /// One package with one string, `hello`.
    fn sample() -> Table {
        let default = config::default();
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
                        config: default,
                        entries: vec![Some(Entry {
                            flags: 0,
                            key: 0,
                            value: string(0),
                        })],
                    }),
                ],
            }],
        }
    }

    #[test]
    fn sets_adds_and_removes() {
        let mut t = sample();
        let (default, night) = (config::default(), config::parse("night").unwrap());
        let hola = t.strings.intern("Hola");
        let p = t.package_mut(0x7f).unwrap();
        let hello = p.find("string", "hello").unwrap();
        assert_eq!(hello, 0x7f01_0000);
        assert_eq!(p.name(hello), Some(("string", "hello")));

        // a value in a new configuration adds its type chunk and marks what the entry varies by
        p.set(hello, &night, string(hola)).unwrap();
        assert_eq!(p.get(hello, &night).unwrap().value, string(hola));
        assert_eq!(p.spec(1).unwrap().flags, [config::bits::UI_MODE]);
        assert_eq!(p.spec(1).unwrap().res1, 2);

        // a new entry gets the next ID in its type, a new type the next type ID
        let other = p.add("string", "other", &default, string(0)).unwrap();
        assert_eq!(other, 0x7f01_0001);
        let flag = EntryValue::Simple(Value {
            ty: Value::INT_BOOLEAN,
            data: u32::MAX,
        });
        let on = p.add("bool", "on", &default, flag.clone()).unwrap();
        assert_eq!(on, 0x7f02_0000);
        assert_eq!(p.add("bool", "on", &night, flag).unwrap(), on);

        assert!(p.remove(hello, &night));
        assert!(!p.remove(hello, &night));
        assert_eq!(p.spec(1).unwrap().flags[0], 0);
        // the string type has no night values left, so no night chunk
        assert_eq!(p.types_of(1).count(), 1);
        assert_eq!(p.spec(1).unwrap().res1, 1);

        // what was written reads back the same, and decodes to the values put in
        let bytes = write(&t);
        let back = read(&bytes).unwrap();
        assert_eq!(back, t);
        let names = crate::res::Names::new(&back);
        let decoded = crate::res::decode_table(&back, &names).values;
        assert!(decoded["values/strings.xml"].contains("<string name=\"other\">Hello</string>"));
        assert!(decoded["values-night/bools.xml"].contains("<bool name=\"on\">true</bool>"));
        assert!(!decoded.contains_key("values-night/strings.xml"));
    }

    #[test]
    fn set_needs_a_resource_that_is_there() {
        let mut t = sample();
        let p = t.package_mut(0x7f).unwrap();
        assert!(matches!(
            p.set(0x7f01_0009, &config::default(), string(0)),
            Err(Error::EntryNotFound(_))
        ));
    }
}
