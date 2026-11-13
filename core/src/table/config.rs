//! `ResTable_config`: the configuration a type's values are for, as raw bytes and as the
//! qualifiers resource directories are named with (`en-rIN`, `night`, `sw600dp-v21`).

use crate::{Error, Result};

/// The size of the configurations aapt2 writes.
pub const SIZE: usize = 64;

/// The default configuration, which matches every device.
pub fn default() -> Vec<u8> {
    let mut c = vec![0; SIZE];
    c[0] = SIZE as u8;
    c
}

/// What a configuration varies by, as `ResTable_config::diff` reports it: the bits of a type
/// spec's flags.
pub mod bits {
    pub const MCC: u32 = 0x0001;
    pub const MNC: u32 = 0x0002;
    pub const LOCALE: u32 = 0x0004;
    pub const TOUCHSCREEN: u32 = 0x0008;
    pub const KEYBOARD: u32 = 0x0010;
    pub const KEYBOARD_HIDDEN: u32 = 0x0020;
    pub const NAVIGATION: u32 = 0x0040;
    pub const ORIENTATION: u32 = 0x0080;
    pub const DENSITY: u32 = 0x0100;
    pub const SCREEN_SIZE: u32 = 0x0200;
    pub const VERSION: u32 = 0x0400;
    pub const SCREEN_LAYOUT: u32 = 0x0800;
    pub const UI_MODE: u32 = 0x1000;
    pub const SMALLEST_SCREEN_SIZE: u32 = 0x2000;
    pub const LAYOUTDIR: u32 = 0x4000;
    pub const SCREEN_ROUND: u32 = 0x8000;
    pub const COLOR_MODE: u32 = 0x10000;
    pub const GRAMMATICAL_GENDER: u32 = 0x20000;
}

/// A field past the end of a shorter, older configuration is 0.
fn byte(c: &[u8], o: usize) -> u8 {
    c.get(o).copied().unwrap_or(0)
}
fn u16_(c: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([byte(c, o), byte(c, o + 1)])
}
fn bytes(c: &[u8], range: std::ops::Range<usize>) -> Vec<u8> {
    range.map(|o| byte(c, o)).collect()
}

/// The bits in which two configurations differ.
pub fn diff(a: &[u8], b: &[u8]) -> u32 {
    use bits::*;
    let field = |o: usize, n: usize| bytes(a, o..o + n) != bytes(b, o..o + n);
    let masked = |o: usize, m: u8| byte(a, o) & m != byte(b, o) & m;
    let mut d = 0;
    let mut set = |cond: bool, bit: u32| {
        if cond {
            d |= bit
        }
    };
    set(field(4, 2), MCC);
    set(field(6, 2), MNC);
    set(field(8, 4) || field(36, 12) || field(53, 8), LOCALE);
    set(field(12, 1), ORIENTATION);
    set(field(13, 1), TOUCHSCREEN);
    set(field(14, 2), DENSITY);
    set(field(16, 1), KEYBOARD);
    set(field(17, 1), NAVIGATION);
    set(masked(18, 0x0f), KEYBOARD_HIDDEN);
    set(field(19, 1), GRAMMATICAL_GENDER);
    set(field(20, 4) || field(32, 4), SCREEN_SIZE);
    set(field(24, 4), VERSION);
    set(masked(28, 0x3f), SCREEN_LAYOUT);
    set(masked(28, 0xc0), LAYOUTDIR);
    set(field(29, 1), UI_MODE);
    set(field(30, 2), SMALLEST_SCREEN_SIZE);
    set(masked(48, 0x03), SCREEN_ROUND);
    set(field(49, 1), COLOR_MODE);
    d
}

/// Names for the values of the one-byte fields: (offset, mask, value, qualifier).
const NAMED: &[(usize, u8, u8, &str)] = &[
    (28, 0xc0, 0x40, "ldltr"),
    (28, 0xc0, 0x80, "ldrtl"),
    (28, 0x0f, 0x01, "small"),
    (28, 0x0f, 0x02, "normal"),
    (28, 0x0f, 0x03, "large"),
    (28, 0x0f, 0x04, "xlarge"),
    (28, 0x30, 0x10, "notlong"),
    (28, 0x30, 0x20, "long"),
    (48, 0x03, 0x01, "notround"),
    (48, 0x03, 0x02, "round"),
    (49, 0x03, 0x01, "nowidecg"),
    (49, 0x03, 0x02, "widecg"),
    (49, 0x0c, 0x04, "lowdr"),
    (49, 0x0c, 0x08, "highdr"),
    (12, 0xff, 0x01, "port"),
    (12, 0xff, 0x02, "land"),
    (12, 0xff, 0x03, "square"),
    (29, 0x0f, 0x01, "normal"),
    (29, 0x0f, 0x02, "desk"),
    (29, 0x0f, 0x03, "car"),
    (29, 0x0f, 0x04, "television"),
    (29, 0x0f, 0x05, "appliance"),
    (29, 0x0f, 0x06, "watch"),
    (29, 0x0f, 0x07, "vrheadset"),
    (29, 0x30, 0x10, "notnight"),
    (29, 0x30, 0x20, "night"),
    (13, 0xff, 0x01, "notouch"),
    (13, 0xff, 0x02, "stylus"),
    (13, 0xff, 0x03, "finger"),
    (18, 0x03, 0x01, "keysexposed"),
    (18, 0x03, 0x02, "keyshidden"),
    (18, 0x03, 0x03, "keyssoft"),
    (16, 0xff, 0x01, "nokeys"),
    (16, 0xff, 0x02, "qwerty"),
    (16, 0xff, 0x03, "12key"),
    (18, 0x0c, 0x04, "navexposed"),
    (18, 0x0c, 0x08, "navhidden"),
    (17, 0xff, 0x01, "nonav"),
    (17, 0xff, 0x02, "dpad"),
    (17, 0xff, 0x03, "trackball"),
    (17, 0xff, 0x04, "wheel"),
    (19, 0xff, 0x01, "neuter"),
    (19, 0xff, 0x02, "feminine"),
    (19, 0xff, 0x03, "masculine"),
];

const DENSITIES: &[(u16, &str)] = &[
    (120, "ldpi"),
    (160, "mdpi"),
    (213, "tvdpi"),
    (240, "hdpi"),
    (320, "xhdpi"),
    (480, "xxhdpi"),
    (640, "xxxhdpi"),
    (0xfffe, "anydpi"),
    (0xffff, "nodpi"),
];

/// A language or region as stored: two letters, or three packed into two bytes.
fn unpack(b: [u8; 2], base: u8) -> String {
    if b[0] & 0x80 == 0 {
        return b.iter().filter(|&&c| c != 0).map(|&c| c as char).collect();
    }
    let first = b[1] & 0x1f;
    let second = ((b[1] & 0xe0) >> 5) | ((b[0] & 0x03) << 3);
    let third = (b[0] & 0x7c) >> 2;
    [first, second, third]
        .iter()
        .map(|&c| (c + base) as char)
        .collect()
}

fn pack(s: &str, base: u8) -> Result<[u8; 2]> {
    let b = s.as_bytes();
    match b.len() {
        2 => Ok([b[0], b[1]]),
        3 => {
            let [first, second, third] = [b[0] - base, b[1] - base, b[2] - base];
            Ok([0x80 | (third << 2) | (second >> 3), (second << 5) | first])
        }
        _ => Err(Error::resources(format!("bad language or region `{s}`"))),
    }
}

/// The qualifiers of a configuration in aapt2's order, joined by `-`; empty for the default.
pub fn name(c: &[u8]) -> String {
    let mut q: Vec<String> = Vec::new();
    let (mcc, mnc) = (u16_(c, 4), u16_(c, 6));
    if mcc != 0 {
        q.push(format!("mcc{mcc}"));
    }
    if mnc != 0 {
        q.push(match mnc {
            0xffff => "mnc00".into(),
            _ => format!("mnc{mnc:02}"),
        });
    }
    let lang = unpack([byte(c, 8), byte(c, 9)], b'a');
    let region = unpack([byte(c, 10), byte(c, 11)], b'0');
    let script: String = bytes(c, 36..40)
        .into_iter()
        .filter(|&b| b != 0)
        .map(char::from)
        .collect();
    let variant: String = bytes(c, 40..48)
        .into_iter()
        .filter(|&b| b != 0)
        .map(char::from)
        .collect();
    if !script.is_empty() || !variant.is_empty() || lang.len() == 3 || region.len() == 3 {
        let mut b = format!("b+{lang}");
        for part in [&script, &region, &variant] {
            if !part.is_empty() {
                b += &format!("+{part}");
            }
        }
        q.push(b);
    } else if !lang.is_empty() {
        q.push(lang);
        if !region.is_empty() {
            q.push(format!("r{region}"));
        }
    }
    let named = |q: &mut Vec<String>, o: usize, m: u8| {
        if let Some(n) = NAMED
            .iter()
            .find(|&&(no, nm, v, _)| no == o && nm == m && byte(c, o) & m == v)
        {
            q.push(n.3.into());
        }
    };
    named(&mut q, 28, 0xc0);
    let (sw, w, h) = (u16_(c, 30), u16_(c, 32), u16_(c, 34));
    if sw != 0 {
        q.push(format!("sw{sw}dp"));
    }
    if w != 0 {
        q.push(format!("w{w}dp"));
    }
    if h != 0 {
        q.push(format!("h{h}dp"));
    }
    named(&mut q, 28, 0x0f);
    named(&mut q, 28, 0x30);
    named(&mut q, 48, 0x03);
    named(&mut q, 49, 0x03);
    named(&mut q, 49, 0x0c);
    named(&mut q, 12, 0xff);
    named(&mut q, 29, 0x0f);
    named(&mut q, 29, 0x30);
    named(&mut q, 19, 0xff);
    match u16_(c, 14) {
        0 => {}
        d => q.push(
            DENSITIES
                .iter()
                .find(|x| x.0 == d)
                .map_or(format!("{d}dpi"), |x| x.1.into()),
        ),
    }
    named(&mut q, 13, 0xff);
    named(&mut q, 18, 0x03);
    named(&mut q, 16, 0xff);
    named(&mut q, 18, 0x0c);
    named(&mut q, 17, 0xff);
    let (sw_px, sh_px) = (u16_(c, 20), u16_(c, 22));
    if sw_px != 0 || sh_px != 0 {
        q.push(format!("{sw_px}x{sh_px}"));
    }
    let (sdk, minor) = (u16_(c, 24), u16_(c, 26));
    if sdk != 0 || minor != 0 {
        q.push(match minor {
            0 => format!("v{sdk}"),
            _ => format!("v{sdk}.{minor}"),
        });
    }
    q.join("-")
}

/// The configuration that qualifiers name, as [`name`] writes them.
pub fn parse(qualifiers: &str) -> Result<Vec<u8>> {
    let mut c = default();
    let bad = |q: &str| Error::resources(format!("unknown qualifier `{q}` in `{qualifiers}`"));
    let set16 = |c: &mut Vec<u8>, o: usize, v: u16| c[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let num = |q: &str, prefix: &str, suffix: &str| -> Option<u16> {
        q.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()
    };
    let mut parts = qualifiers.split('-').filter(|q| !q.is_empty()).peekable();
    while let Some(q) = parts.next() {
        if let Some(v) = num(q, "mcc", "") {
            set16(&mut c, 4, v);
        } else if let Some(v) = num(q, "mnc", "") {
            set16(&mut c, 6, if v == 0 { 0xffff } else { v });
        } else if let Some(b) = q.strip_prefix("b+") {
            let mut tags = b.split('+');
            let lang = tags.next().unwrap_or_default();
            c[8..10].copy_from_slice(&pack(lang, b'a')?);
            for tag in tags {
                if tag.len() == 4 {
                    c[36..40].copy_from_slice(tag.as_bytes());
                } else if tag.len() <= 3 && tag.len() >= 2 {
                    c[10..12].copy_from_slice(&pack(tag, b'0')?);
                } else if tag.len() <= 8 {
                    c[40..40 + tag.len()].copy_from_slice(tag.as_bytes());
                } else {
                    return Err(bad(q));
                }
            }
        } else if q.len() == 2 && q.bytes().all(|b| b.is_ascii_lowercase()) && c[8] == 0 {
            c[8..10].copy_from_slice(q.as_bytes());
            if let Some(r) = parts
                .peek()
                .and_then(|r| r.strip_prefix('r'))
                .filter(|r| r.len() == 2)
            {
                c[10..12].copy_from_slice(r.as_bytes());
                parts.next();
            }
        } else if let Some(v) = num(q, "sw", "dp") {
            set16(&mut c, 30, v);
        } else if let Some(v) = num(q, "w", "dp") {
            set16(&mut c, 32, v);
        } else if let Some(v) = num(q, "h", "dp") {
            set16(&mut c, 34, v);
        } else if let Some(d) = DENSITIES.iter().find(|d| d.1 == q) {
            set16(&mut c, 14, d.0);
        } else if let Some(v) = num(q, "", "dpi") {
            set16(&mut c, 14, v);
        } else if let Some(v) = q.strip_prefix('v') {
            let (sdk, minor) = v.split_once('.').unwrap_or((v, "0"));
            set16(&mut c, 24, sdk.parse().map_err(|_| bad(q))?);
            set16(&mut c, 26, minor.parse().map_err(|_| bad(q))?);
        } else if let Some((w, h)) = q
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        {
            set16(&mut c, 20, w);
            set16(&mut c, 22, h);
        } else {
            // the byte fields, in order: `normal` is a screen size before a UI mode type
            let taken = |c: &[u8], o: usize, m: u8| c[o] & m != 0;
            let n = NAMED
                .iter()
                .find(|&&(o, m, _, name)| name == q && !taken(&c, o, m))
                .ok_or_else(|| bad(q))?;
            c[n.0] = (c[n.0] & !n.1) | n.2;
        }
    }
    Ok(c)
}
