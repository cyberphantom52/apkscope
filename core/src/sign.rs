//! APK signatures: find the v2/v3 signing block and v1 (JAR) signature files, and read the
//! signer certificates (subject, issuer, validity, fingerprints).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const V2_ID: u32 = 0x7109_871a;
pub const V3_ID: u32 = 0xf053_68c0;
pub const V31_ID: u32 = 0x1b93_ad61;

// ---------- minimal DER ----------

/// (tag, content, rest) of the DER element at the start of `d`.
fn der(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *d.first()?;
    let first = *d.get(1)? as usize;
    let (len, head) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        (
            d.get(2..2 + n)?
                .iter()
                .fold(0usize, |a, &b| (a << 8) | b as usize),
            2 + n,
        )
    };
    Some((tag, d.get(head..head + len)?, d.get(head + len..)?))
}

/// Children of a constructed element.
fn children(mut d: &[u8]) -> Vec<(u8, &[u8])> {
    let mut v = Vec::new();
    while let Some((t, c, rest)) = der(d) {
        v.push((t, c));
        d = rest;
    }
    v
}

fn oid(c: &[u8]) -> String {
    let mut parts = Vec::new();
    if let Some(&f) = c.first() {
        parts.push((f / 40) as u64);
        parts.push((f % 40) as u64);
    }
    let mut v = 0u64;
    for &b in c.iter().skip(1) {
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            parts.push(v);
            v = 0;
        }
    }
    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// `CN=..., O=..., C=...` of an X.501 Name.
fn name(c: &[u8]) -> String {
    let mut out = Vec::new();
    for (_, set) in children(c) {
        for (_, atv) in children(set) {
            let kids = children(atv);
            let (Some((_, o)), Some((_, v))) = (kids.first(), kids.get(1)) else {
                continue;
            };
            let key = match oid(o).as_str() {
                "2.5.4.3" => "CN",
                "2.5.4.6" => "C",
                "2.5.4.7" => "L",
                "2.5.4.8" => "ST",
                "2.5.4.10" => "O",
                "2.5.4.11" => "OU",
                "1.2.840.113549.1.9.1" => "E",
                other => {
                    out.push(format!("{other}={}", String::from_utf8_lossy(v)));
                    continue;
                }
            };
            out.push(format!("{key}={}", String::from_utf8_lossy(v)));
        }
    }
    out.join(", ")
}

/// UTCTime / GeneralizedTime as `YYYY-MM-DD`.
fn time(tag: u8, c: &[u8]) -> String {
    let s = String::from_utf8_lossy(c);
    match tag {
        0x17 if s.len() >= 6 => {
            let yy: u32 = s[..2].parse().unwrap_or(0);
            format!(
                "{}-{}-{}",
                if yy < 50 { 2000 + yy } else { 1900 + yy },
                &s[2..4],
                &s[4..6]
            )
        }
        0x18 if s.len() >= 8 => format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..8]),
        _ => s.into_owned(),
    }
}

pub struct Cert {
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    pub key: String,
    pub sha256: String,
    pub sha1: String,
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_cert(der_bytes: &[u8]) -> Option<Cert> {
    let (_, cert, _) = der(der_bytes)?;
    let (_, tbs) = *children(cert).first()?;
    let mut f = children(tbs);
    // optional explicit [0] version
    if f.first().is_some_and(|x| x.0 == 0xa0) {
        f.remove(0);
    }
    let issuer = name(f.get(2)?.1);
    let validity = children(f.get(3)?.1);
    let subject = name(f.get(4)?.1);
    let alg = children(f.get(5)?.1)
        .first()
        .and_then(|(_, a)| children(a).first().map(|o| oid(o.1)));
    let key = match alg.as_deref() {
        Some("1.2.840.113549.1.1.1") => "RSA",
        Some("1.2.840.10045.2.1") => "EC",
        Some("1.2.840.10040.4.1") => "DSA",
        _ => "unknown",
    };
    Some(Cert {
        subject,
        issuer,
        not_before: validity.first().map(|v| time(v.0, v.1)).unwrap_or_default(),
        not_after: validity.get(1).map(|v| time(v.0, v.1)).unwrap_or_default(),
        key: key.into(),
        sha256: hex(&Sha256::digest(der_bytes)),
        sha1: hex(&Sha1::digest(der_bytes)),
    })
}

/// First certificate inside a PKCS#7 SignedData (a v1 `META-INF/*.RSA|DSA|EC` file).
pub fn pkcs7_cert(d: &[u8]) -> Option<Vec<u8>> {
    let (_, ci, _) = der(d)?;
    let content = children(ci);
    let (_, explicit) = content.get(1)?;
    let (_, signed, _) = der(explicit)?;
    let (_, certs) = children(signed).into_iter().find(|x| x.0 == 0xa0)?;
    let (tag, body, _) = der(certs)?;
    // re-encode the first certificate with its header
    let total = certs.len() - der(certs).map_or(0, |x| x.2.len());
    (tag == 0x30)
        .then(|| certs[..total].to_vec())
        .filter(|_| !body.is_empty())
}

// ---------- APK signing block ----------

/// Offset of the central directory, from the end-of-central-directory record.
pub fn central_dir(f: &mut File) -> Option<u64> {
    let len = f.seek(SeekFrom::End(0)).ok()?;
    let tail = len.min(65_557);
    f.seek(SeekFrom::Start(len - tail)).ok()?;
    let mut buf = vec![0; tail as usize];
    f.read_exact(&mut buf).ok()?;
    let at = (0..buf.len().saturating_sub(21))
        .rev()
        .find(|&i| buf[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])?;
    Some(u32::from_le_bytes(buf[at + 16..at + 20].try_into().ok()?) as u64)
}

/// The APK Signing Block's (id, value) pairs, if the APK has one.
pub fn signing_block(path: &str) -> Option<Vec<(u32, Vec<u8>)>> {
    let mut f = File::open(path).ok()?;
    let cd = central_dir(&mut f)?;
    if cd < 32 {
        return None;
    }
    let mut foot = [0u8; 24];
    f.seek(SeekFrom::Start(cd - 24)).ok()?;
    f.read_exact(&mut foot).ok()?;
    if &foot[8..] != b"APK Sig Block 42" {
        return None;
    }
    let size = u64::from_le_bytes(foot[..8].try_into().ok()?);
    if size > cd || size < 24 {
        return None;
    }
    let start = cd - size - 8;
    let mut block = vec![0; (size + 8) as usize];
    f.seek(SeekFrom::Start(start)).ok()?;
    f.read_exact(&mut block).ok()?;
    let pairs_end = block.len() - 24;
    let mut i = 8;
    let mut out = Vec::new();
    while i + 12 <= pairs_end {
        let len = u64::from_le_bytes(block[i..i + 8].try_into().ok()?) as usize;
        let id = u32::from_le_bytes(block[i + 8..i + 12].try_into().ok()?);
        let end = (i + 8 + len).min(pairs_end);
        out.push((id, block.get(i + 12..end)?.to_vec()));
        i = end;
    }
    Some(out)
}

/// u32-length-prefixed items.
fn prefixed(mut d: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    while d.len() >= 4 {
        let n = u32::from_le_bytes(d[..4].try_into().unwrap()) as usize;
        let Some(item) = d.get(4..4 + n) else { break };
        v.push(item);
        d = &d[4 + n..];
    }
    v
}

/// First signer certificate of a v2/v3 scheme value.
pub fn scheme_cert(value: &[u8]) -> Option<Vec<u8>> {
    // value: length-prefixed sequence of length-prefixed signers
    let seq = prefixed(value);
    let signers = prefixed(seq.first()?);
    let signer = prefixed(signers.first()?);
    let signed = prefixed(signer.first()?);
    // signed data: digests, certificates, ...
    let certs = prefixed(signed.get(1)?);
    certs.first().map(|c| c.to_vec())
}

/// Readable summary for the overview.
pub fn summary(
    apk_path: &str,
    entries: &[String],
    mut read: impl FnMut(&str) -> Option<Vec<u8>>,
) -> String {
    let mut o = String::new();
    let block = signing_block(apk_path).unwrap_or_default();
    let has = |id| block.iter().any(|b| b.0 == id);
    let v1 = entries.iter().find(|e| {
        e.starts_with("META-INF/")
            && [".RSA", ".DSA", ".EC"]
                .iter()
                .any(|x| e.to_uppercase().ends_with(x))
    });
    let schemes: Vec<&str> = [
        (v1.is_some(), "v1 (JAR)"),
        (has(V2_ID), "v2"),
        (has(V3_ID), "v3"),
        (has(V31_ID), "v3.1"),
    ]
    .into_iter()
    .filter(|x| x.0)
    .map(|x| x.1)
    .collect();
    o += &format!(
        "schemes           {}\n",
        if schemes.is_empty() {
            "none (unsigned)".to_string()
        } else {
            schemes.join(", ")
        }
    );
    // newest scheme first; fall back to the v1 signature file
    let from_block = [V31_ID, V3_ID, V2_ID]
        .iter()
        .filter_map(|id| block.iter().find(|b| b.0 == *id))
        .filter_map(|b| scheme_cert(&b.1))
        .find_map(|c| parse_cert(&c));
    let cert = from_block.or_else(|| {
        v1.and_then(|p| read(p))
            .and_then(|d| pkcs7_cert(&d))
            .and_then(|c| parse_cert(&c))
    });
    match cert {
        Some(c) => {
            o += &format!(
                "subject           {}\nissuer            {}\nvalid             {} to {}\nkey               {}\nSHA-256           {}\nSHA-1             {}\n",
                c.subject, c.issuer, c.not_before, c.not_after, c.key, c.sha256, c.sha1
            );
            if c.subject.contains("Android Debug") {
                o += "                  <- debug certificate\n";
            }
        }
        None if !schemes.is_empty() => o += "certificate       could not be read\n",
        None => {}
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_and_oids() {
        // SEQUENCE { INTEGER 5 } then a trailing byte
        let (t, c, rest) = der(&[0x30, 0x03, 0x02, 0x01, 0x05, 0xff]).unwrap();
        assert_eq!((t, c, rest), (0x30, &[0x02, 0x01, 0x05][..], &[0xff][..]));
        // long-form length
        let long = [&[0x04, 0x81, 0x80][..], &[0u8; 0x80]].concat();
        assert_eq!(der(&long).unwrap().1.len(), 0x80);
        assert_eq!(oid(&[0x55, 0x04, 0x03]), "2.5.4.3");
        assert_eq!(
            oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]),
            "1.2.840.113549.1.1.1"
        );
        assert_eq!(time(0x17, b"240131120000Z"), "2024-01-31");
    }

    /// Reads the certificate of the local test APKs when they are present (skipped elsewhere).
    #[test]
    fn real_apk_certificates() {
        for apk in ["D:/APKS/Zestle_v0.6.8-prealpha.apk", "D:/APKS/Unciv.apk"] {
            if !std::path::Path::new(apk).exists() {
                continue;
            }
            let block = signing_block(apk).expect("signing block");
            let v2 = block.iter().find(|b| b.0 == V2_ID).expect("v2 scheme");
            let c = parse_cert(&scheme_cert(&v2.1).expect("v2 cert")).expect("parsed cert");
            assert!(
                !c.subject.is_empty() && c.sha256.len() == 95,
                "{apk}: {} / {}",
                c.subject,
                c.sha256
            );
            eprintln!(
                "{apk}: {} | {} .. {} | {}",
                c.subject, c.not_before, c.not_after, c.sha256
            );
        }
    }

    #[test]
    fn length_prefixed() {
        let d = [2, 0, 0, 0, 7, 8, 1, 0, 0, 0, 9];
        assert_eq!(prefixed(&d), vec![&[7, 8][..], &[9][..]]);
    }
}
