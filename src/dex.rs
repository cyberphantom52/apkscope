//! The DEX reader of the core crate, with class names as the user sees them.

pub use apkscope_core::dex::*;

/// `La/b/C;` -> `a.b.C`, under its renamed or deobfuscated name.
pub fn java_name(desc: &str) -> String {
    let desc = crate::deobf::class(desc).unwrap_or(desc);
    desc.strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(desc)
        .replace('/', ".")
}

#[cfg(test)]
pub(crate) mod tests {

    /// hand-built DEX: 1 string, 1 type, 1 class_def `a.b.Cls`
    pub(crate) fn tiny_dex() -> Vec<u8> {
        let mut d = vec![0u8; 0x70];
        d[..8].copy_from_slice(b"dex\n035\0");
        let put =
            |d: &mut Vec<u8>, off: usize, v: u32| d[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut d, 0x3C, 0x70); // string_ids
        put(&mut d, 0x44, 0x74); // type_ids
        put(&mut d, 0x60, 1);
        put(&mut d, 0x64, 0x78); // class_defs
        d.resize(0x78 + 32, 0);
        put(&mut d, 0x70, 0x98); // string data offset
        let s = b"\x0aLa/b/Cls;\0";
        d.extend_from_slice(s);
        d
    }
}
