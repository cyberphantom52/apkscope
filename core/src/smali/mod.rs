//! Smali round trip: DEX -> class model -> Smali text, and back to a DEX file.
//! Only used by editing commands; the query paths never load it.

pub mod model;
pub mod read;
pub mod text;
pub mod write;

/// The 3-digit version of a DEX file (`035`, `038`, ...).
pub fn version(dex: &[u8]) -> [u8; 3] {
    dex.get(4..7)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(*b"035")
}

use model::Class;

/// `classesN.dex` entries, in load order (classes.dex, classes2.dex, ...).
fn dex_entries(apk: &crate::apk::Apk) -> Vec<String> {
    let mut v: Vec<String> = apk
        .zip
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
        .collect();
    v.sort_by_key(|n| (n.len(), n.clone())); // classes.dex before classes2.dex ... classes10.dex
    v
}

/// Round-trippable Smali of one class (dotted or `L..;` name), for editing then re-assembling.
pub fn export_class(path: &str, class: &str) -> Result<String, String> {
    let desc = crate::dex::descriptor(class);
    let mut apk = crate::apk::open(path)?;
    for name in dex_entries(&apk) {
        let Some(bytes) = apk.read(&name) else {
            continue;
        };
        if let Ok(Some(i)) = crate::dex::Dex::parse(&bytes).map(|dx| dx.find_class(&desc)) {
            let c = read::Reader::new(&bytes)
                .and_then(|r| r.class(i))
                .map_err(|e| format!("{name}: {e}"))?;
            return Ok(text::print(&c));
        }
    }
    Err(format!("class {class} not found"))
}

/// Assemble edited Smali `sources` into `edits`: each class replaces the one of the same name in
/// the DEX that defines it (a brand-new class goes to classes.dex). Returns the changed entries.
/// Parses everything first, so a syntax error aborts before any DEX is rewritten.
pub fn inject(
    path: &str,
    sources: &[String],
    edits: &mut crate::rebuild::Edits,
) -> Result<Vec<String>, String> {
    let classes: Vec<Class> = sources
        .iter()
        .map(|s| text::parse(s))
        .collect::<Result<_, _>>()?;
    let mut apk = crate::apk::open(path)?;
    let entries = dex_entries(&apk);
    // group each edited class under the dex entry it belongs to
    let mut by_dex: std::collections::BTreeMap<String, Vec<Class>> = Default::default();
    'next: for c in classes {
        let desc = String::from_utf8_lossy(&c.name).into_owned();
        for name in &entries {
            if let Some(bytes) = apk.read(name)
                && crate::dex::Dex::parse(&bytes)
                    .ok()
                    .and_then(|dx| dx.find_class(&desc))
                    .is_some()
            {
                by_dex.entry(name.clone()).or_default().push(c);
                continue 'next;
            }
        }
        by_dex.entry("classes.dex".into()).or_default().push(c);
    }
    let mut changed = Vec::new();
    for (name, edited) in by_dex {
        let bytes = apk
            .read(&name)
            .ok_or_else(|| format!("{name} not in the APK"))?;
        // fast path: patch instruction bytes in place when the edits only change existing methods'
        // code, so a one-class edit doesn't model and rewrite the whole DEX
        let patched = {
            let dx = crate::dex::Dex::parse(&bytes).map_err(|e| format!("{name}: {e}"))?;
            let indexed: Option<Vec<(usize, &Class)>> = edited
                .iter()
                .map(|c| {
                    dx.find_class(&String::from_utf8_lossy(&c.name))
                        .map(|k| (k, c))
                })
                .collect();
            match indexed {
                Some(ix) => write::patch(&bytes, &ix).map_err(|e| format!("{name}: {e}"))?,
                None => None, // a brand-new class: no in-place slot
            }
        };
        let dex = match patched {
            Some(d) => d,
            None => {
                let mut all = read::read(&bytes).map_err(|e| format!("{name}: {e}"))?;
                for c in edited {
                    match all.iter().position(|x| x.name == c.name) {
                        Some(k) => all[k] = c,
                        None => all.push(c),
                    }
                }
                write::write(&all, version(&bytes)).map_err(|e| format!("{name}: {e}"))?
            }
        };
        edits.insert(name.clone(), Some(dex));
        changed.push(name);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::Dex;

    /// written DEX -> model -> written again must give the same bytes, also through Smali text
    fn roundtrip(name: &str, orig: &[u8]) -> (usize, usize) {
        let classes = read::read(orig).unwrap_or_else(|e| panic!("{name}: read: {e}"));
        let d1 =
            write::write(&classes, version(orig)).unwrap_or_else(|e| panic!("{name}: write: {e}"));
        let c1 = read::read(&d1).unwrap_or_else(|e| panic!("{name}: re-read: {e}"));
        let again = write::write(&c1, version(orig)).unwrap();
        if d1 != again {
            let by: std::collections::HashMap<&[u8], &model::Class> =
                c1.iter().map(|c| (&*c.name, c)).collect();
            for a in &classes {
                let b = by[&*a.name];
                if a != b {
                    for (ma, mb) in a.methods.iter().zip(&b.methods) {
                        if ma != mb {
                            let (Some(ca), Some(cb)) = (ma.code.as_ref(), mb.code.as_ref()) else {
                                panic!(
                                    "{name}: {:?}
{:?}",
                                    ma, mb
                                )
                            };
                            let one = |m: &model::Method| {
                                text::print(&model::Class {
                                    methods: vec![m.clone()],
                                    fields: vec![],
                                    ..a.clone()
                                })
                            };
                            std::fs::write("rt_a.smali", one(ma)).unwrap();
                            std::fs::write("rt_b.smali", one(mb)).unwrap();
                            for (x, y) in ca.items.iter().zip(&cb.items) {
                                assert_eq!(
                                    x,
                                    y,
                                    "{name}: {} {}",
                                    String::from_utf8_lossy(&a.name),
                                    String::from_utf8_lossy(&ma.name)
                                );
                            }
                            assert_eq!(ca, cb);
                        }
                    }
                    assert_eq!(a, b, "{name}: writing is not stable");
                }
            }
            let at = 0x20
                + d1[0x20..]
                    .iter()
                    .zip(&again[0x20..])
                    .position(|(x, y)| x != y)
                    .unwrap_or(0);
            let map = u32::from_le_bytes(d1[0x34..0x38].try_into().unwrap()) as usize;
            let n = u32::from_le_bytes(d1[map..map + 4].try_into().unwrap()) as usize;
            let secs: Vec<String> = (0..n)
                .map(|k| {
                    let o = map + 4 + k * 12;
                    format!(
                        "{:#x}@{:#x}",
                        u16::from_le_bytes(d1[o..o + 2].try_into().unwrap()),
                        u32::from_le_bytes(d1[o + 8..o + 12].try_into().unwrap())
                    )
                })
                .collect();
            panic!(
                "{name}: writing is not stable: first diff at {at:#x} of {:#x}/{:#x}; sections {secs:?}",
                d1.len(),
                again.len()
            );
        }
        let c2: Vec<_> = c1
            .iter()
            .map(|c| {
                let t = text::print(c);
                text::parse(&t).unwrap_or_else(|e| {
                    panic!("{name}: {}: {e}\n{t}", String::from_utf8_lossy(&c.name))
                })
            })
            .collect();
        let d2 = write::write(&c2, version(orig)).unwrap();
        if d1 != d2 {
            for (a, b) in c1.iter().zip(&c2) {
                if a != b {
                    for (ma, mb) in a.methods.iter().zip(&b.methods) {
                        assert_eq!(
                            ma,
                            mb,
                            "{name}: {} differs after Smali\n{}",
                            String::from_utf8_lossy(&a.name),
                            text::print(a)
                        );
                    }
                    assert_eq!(a, b, "{name}: class differs after Smali");
                }
            }
            panic!("{name}: Smali round trip changed the DEX");
        }
        same_meaning(name, orig, &d1);
        (classes.len(), d1.len())
    }

    /// the rewritten DEX lists the same classes, members and instructions as the original
    fn same_meaning(name: &str, a: &[u8], b: &[u8]) {
        let (x, y) = (Dex::parse(a).unwrap(), Dex::parse(b).unwrap());
        assert_eq!(x.class_count, y.class_count, "{name}: class count");
        let desc = |d: &Dex, i| d.class_descriptor(i).unwrap();
        let body = |d: &Dex, i: usize| -> Vec<String> {
            let cd = d.class_def(i).unwrap();
            let mut v = vec![format!(
                "{} {:x} {:?} {:?} {:?}",
                cd.class, cd.access, cd.superclass, cd.interfaces, cd.source_file
            )];
            let data = d.class_data(i).unwrap();
            for (f, a) in data.fields {
                v.push(format!("F {} {a:x}", d.field_ref(f).unwrap()));
            }
            for m in data.methods {
                v.push(format!(
                    "M {} {:x}",
                    d.method_ref(m.method_idx).unwrap(),
                    m.access
                ));
                if let Some(c) = m.code {
                    v.push(format!(
                        "  regs {} ins {} tries {}",
                        c.registers,
                        c.ins,
                        c.tries.len()
                    ));
                    for i in crate::bytecode::decode(c.insns).unwrap() {
                        v.push(crate::dex::render(d, &i).unwrap_or_else(|e| e));
                    }
                }
            }
            v.push(
                format!("{:?}", d.static_values(i).unwrap())
                    .chars()
                    .filter(|c| !c.is_ascii_digit())
                    .collect(),
            );
            v
        };
        let mut by: std::collections::HashMap<String, usize> =
            (0..y.class_count).map(|i| (desc(&y, i), i)).collect();
        for i in 0..x.class_count {
            let j = by
                .remove(&desc(&x, i))
                .unwrap_or_else(|| panic!("{name}: {} missing", desc(&x, i)));
            let (p, q) = (body(&x, i), body(&y, j));
            for (l1, l2) in p.iter().zip(&q) {
                // branch offsets print as numbers and stay equal unless code grew
                assert_eq!(l1, l2, "{name}: {} differs", desc(&x, i));
            }
            assert_eq!(
                p.len(),
                q.len(),
                "{name}: {} differs in length",
                desc(&x, i)
            );
        }
    }

    #[test]
    fn assembles_hand_written_smali() {
        let src = r#"
.class public Lcom/example/Hello;
.super Ljava/lang/Object;
.source "Hello.java"

.field private static final GREETING:Ljava/lang/String; = "hi \"there\"\n"
.field public count:I

.method public constructor <init>()V
    .locals 0
    invoke-direct {p0}, Ljava/lang/Object;-><init>()V
    return-void
.end method

.method public static pick(I)I
    .locals 1
    .param p0, "which"
    .line 10
    packed-switch p0, :pswitch_data_0
    const/4 v0, -0x1
    :goto_0
    return v0
    :pswitch_0
    const/16 v0, 0x64
    goto :goto_0
    :pswitch_1
    :try_start_0
    const-string v0, "x"
    invoke-static {v0}, Ljava/lang/Integer;->parseInt(Ljava/lang/String;)I
    move-result v0
    :try_end_0
    .catch Ljava/lang/NumberFormatException; {:try_start_0 .. :try_end_0} :catch_0
    goto :goto_0
    :catch_0
    const/4 v0, 0x0
    goto :goto_0
    :pswitch_data_0
    .packed-switch 0x1
        :pswitch_0
        :pswitch_1
    .end packed-switch
.end method
"#;
        let c = text::parse(src).unwrap();
        let d = write::write(&[c.clone()], *b"035").unwrap();
        let x = Dex::parse(&d).unwrap();
        assert_eq!(x.class_descriptor(0).unwrap(), "Lcom/example/Hello;");
        let back = read::read(&d).unwrap();
        // the printed form of what was written parses back to the same DEX
        let again = write::write(&[text::parse(&text::print(&back[0])).unwrap()], *b"035").unwrap();
        assert_eq!(d, again);
        assert_eq!(
            back[0].fields[0].init,
            Some(model::Value::Str(model::s(b"hi \"there\"\n")))
        );
        let m = x.methods(0).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].code.as_ref().unwrap().tries.len(), 1);
        // DEX header checksum verifies
        let mut sealed = d.clone();
        write::seal(&mut sealed);
        assert_eq!(sealed, d);
    }

    #[test]
    fn reports_errors_with_line() {
        let e = text::parse(".class LA;\n.super Ljava/lang/Object;\n.method static f()V\n    .locals 0\n    bogus-op v0\n.end method\n").unwrap_err();
        assert!(e.starts_with("line 5:"), "{e}");
        let e = text::parse(
            ".class LA;\n.method static f()V\n    .locals 0\n    goto :nowhere\n.end method\n",
        )
        .unwrap_err();
        assert!(e.contains(":nowhere"), "{e}");
    }

    #[test]
    fn patches_instruction_in_place() {
        let src = ".class public Lcom/ex/P;\n.super Ljava/lang/Object;\n\
                   .method public static f()I\n    .registers 1\n    const/16 v0, 0x64\n    return v0\n.end method\n";
        let dex = write::write(&[text::parse(src).unwrap()], *b"035").unwrap();
        let ci = Dex::parse(&dex).unwrap().find_class("Lcom/ex/P;").unwrap();

        // same-width immediate change: patched in place, same size, only the operand (and header
        // checksum/signature) differ, and it decodes to the new value
        let edited = text::parse(&src.replace("0x64", "0x65")).unwrap();
        let patched = write::patch(&dex, &[(ci, &edited)])
            .unwrap()
            .expect("same-width edit should patch in place");
        assert_eq!(patched.len(), dex.len());
        let back = read::read(&patched).unwrap();
        let lit = back[0].methods[0]
            .code
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find_map(|it| match it {
                model::Item::Ins(i) if i.op == 0x13 => Some(i.lit),
                _ => None,
            })
            .unwrap();
        assert_eq!(lit, 0x65);

        // adding an instruction changes the layout, so it must fall back (returns None)
        let grew = text::parse(&src.replace("    return v0", "    nop\n    return v0")).unwrap();
        assert!(write::patch(&dex, &[(ci, &grew)]).unwrap().is_none());
    }

    /// APKSCOPE_ROUNDTRIP=<dir of .apk> cargo test --release roundtrip_apks -- --ignored --nocapture
    #[test]
    #[ignore]
    fn roundtrip_apks() {
        let dir = std::env::var("APKSCOPE_ROUNDTRIP").expect("set APKSCOPE_ROUNDTRIP");
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.extension().is_none_or(|x| x != "apk") {
                continue;
            }
            let mut z = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
            let names: Vec<String> = z
                .file_names()
                .filter_map(|n| n.ok().map(|n| n.into_owned()))
                .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
                .collect();
            let t = std::time::Instant::now();
            let (mut classes, mut bytes, mut orig) = (0, 0, 0);
            for n in names {
                let mut b = vec![];
                std::io::Read::read_to_end(&mut z.by_name(&n).unwrap(), &mut b).unwrap();
                let (c, size) = roundtrip(&format!("{}!{n}", path.display()), &b);
                classes += c;
                bytes += size;
                orig += b.len();
            }
            println!(
                "{}: {classes} classes, {orig} -> {bytes} bytes, {:?}",
                path.file_name().unwrap().to_string_lossy(),
                t.elapsed()
            );
        }
    }
}
