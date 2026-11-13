//! Structured Java output: class skeleton, then each method's control flow over the expression tree.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use crate::bytecode::{self, Insn};
use crate::cfg::{self, Cfg, Term};
use crate::dex::{self, Dex};
use crate::imports::type_ref;
use crate::ir::{self, Ctx, Expr, jtype};
use crate::opt::{self, Bir};

type R<T> = Result<T, String>;

fn modifiers(access: u32, method: bool) -> String {
    let mut s = String::new();
    let flags: &[(u32, &str)] = &[
        (0x1, "public"),
        (0x2, "private"),
        (0x4, "protected"),
        (0x8, "static"),
        (0x10, "final"),
        (0x400, "abstract"),
    ];
    for &(bit, name) in flags {
        if access & bit != 0 {
            s += name;
            s.push(' ');
        }
    }
    if method && access & 0x20 != 0 {
        s += "synchronized "
    }
    if method && access & 0x100 != 0 {
        s += "native "
    }
    if !method && access & 0x40 != 0 {
        s += "volatile "
    }
    if !method && access & 0x80 != 0 {
        s += "transient "
    }
    s
}

/// A top-level class as a `.java` file, with its inner classes nested inside.
pub fn class(dex: &Dex, c: usize) -> R<String> {
    let desc = dex.class_descriptor(c)?;
    crate::imports::set_current(&desc);
    let own = crate::imports::source_path(&desc);
    let (pkg, name) = own.split_once('/').unwrap_or(("", &own));
    // a top-level class's name may legally contain `$` (it has no outer class here)
    let name = name.rsplit('.').next().unwrap_or(name);
    INLINED.with(|i| i.borrow_mut().clear());
    let fam = dex
        .descendants(c)
        .into_iter()
        .map(|k| (k, dex.parent_of(k)))
        .collect();
    FAMILY.with(|f| *f.borrow_mut() = (dex as *const Dex as usize, c, fam));
    let body = class_body(dex, c, name)?;
    let text = if pkg.is_empty() {
        body
    } else {
        format!("package {pkg};\n\n{body}")
    };
    Ok(crate::imports::resolve(&text, pkg, &own))
}

/// Name of an inner class inside its outer one: `a/B$C` in `a/B` -> `C`, `a/B$1` -> `AnonymousClass1`.
pub fn inner_name(desc: &str, outer: &str) -> String {
    let rest = desc
        .trim_end_matches(';')
        .get(outer.trim_end_matches(';').len() + 1..)
        .unwrap_or(desc);
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        format!("AnonymousClass{rest}")
    } else {
        rest.to_string()
    }
}

/// Annotations that are compiler bookkeeping, never written in source.
const HIDDEN: [&str; 2] = [
    "Lkotlin/Metadata;",
    "Lkotlin/jvm/internal/SourceDebugExtension;",
];

fn system<'x>(anns: &'x [dex::Annotation], dex: &Dex, desc: &str) -> Option<&'x dex::Annotation> {
    anns.iter()
        .find(|a| a.visibility == 2 && dex.type_name(a.type_idx).is_ok_and(|t| t == desc))
}

fn element<'x>(a: &'x dex::Annotation, dex: &Dex, name: &str) -> Option<&'x dex::Value> {
    a.elements
        .iter()
        .find(|(n, _)| dex.string(*n).is_ok_and(|s| s == name))
        .map(|e| &e.1)
}

fn value_text(dex: &Dex, v: &dex::Value) -> String {
    use dex::Value::*;
    match v {
        Int(x, 'c') => match char::from_u32(*x as u32) {
            Some(c) if !c.is_control() && c != '\'' && c != '\\' => format!("'{c}'"),
            _ => format!("(char) {x}"),
        },
        Int(x, 'j') => format!("{x}L"),
        Int(x, _) => x.to_string(),
        Float(f) => format!("{f:?}f"),
        Double(d) => format!("{d:?}"),
        Str(i) => dex
            .string(*i)
            .map_or("null".into(), |s| ir::java_string(&s)),
        Type(t) => dex
            .type_name(*t)
            .map_or("null".into(), |t| format!("{}.class", type_ref(&t))),
        Field(f) | Enum(f) => dex.field_parts(*f).map_or("null".into(), |(c, ty, n)| {
            format!(
                "{}.{}",
                type_ref(&c),
                crate::deobf::field(&c, &n, &ty).unwrap_or(n)
            )
        }),
        Method(m) => dex
            .method_ref(*m)
            .map_or("null".into(), |r| format!("null /* {r} */")),
        MethodType(_) | MethodHandle(_) => "null /* method handle */".into(),
        Array(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|x| value_text(dex, x))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Annotation(a) => annotation_text(dex, a).unwrap_or_default(),
        Null => "null".into(),
        Bool(b) => b.to_string(),
    }
}

/// A static field's constant, written for the field's declared type (`0` -> `false` / `null` / `0.0f`).
fn typed_value(dex: &Dex, v: &dex::Value, ty: &str) -> String {
    use dex::Value::*;
    match (v, ty) {
        (Int(x, _), "Z") => (*x != 0).to_string(),
        (Int(x, _), "J") => format!("{x}L"),
        (Int(x, _), "F") => format!("{:?}f", *x as f32),
        (Int(x, _), "D") => format!("{:?}", *x as f64),
        (Int(x, _), "C") => value_text(dex, &Int(*x, 'c')),
        (Int(x, k), "B") if *k != 'b' => format!("(byte) {x}"),
        (Int(x, k), "S") if *k != 's' => format!("(short) {x}"),
        (Float(f), _) if f.is_nan() => "Float.NaN".into(),
        (Double(d), _) if d.is_nan() => "Double.NaN".into(),
        (Float(f), _) if f.is_infinite() => format!(
            "Float.{}_INFINITY",
            if *f > 0.0 { "POSITIVE" } else { "NEGATIVE" }
        ),
        (Double(d), _) if d.is_infinite() => format!(
            "Double.{}_INFINITY",
            if *d > 0.0 { "POSITIVE" } else { "NEGATIVE" }
        ),
        _ => value_text(dex, v),
    }
}

/// `@Name(key = value)`; None for system and compiler-bookkeeping annotations
fn annotation_text(dex: &Dex, a: &dex::Annotation) -> Option<String> {
    let desc = dex.type_name(a.type_idx).ok()?;
    if a.visibility == 2 || HIDDEN.contains(&desc.as_str()) {
        return None;
    }
    let mut s = format!("@{}", type_ref(&desc));
    let els: Vec<(String, String)> = a
        .elements
        .iter()
        .map(|(n, v)| (dex.string(*n).unwrap_or_default(), value_text(dex, v)))
        .collect();
    match els.as_slice() {
        [] => {}
        [(n, v)] if n == "value" => s += &format!("({v})"),
        _ => {
            s += &format!(
                "({})",
                els.iter()
                    .map(|(n, v)| format!("{n} = {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
    Some(s)
}

fn annotation_lines(dex: &Dex, anns: &[dex::Annotation], indent: &str) -> String {
    anns.iter()
        .filter_map(|a| annotation_text(dex, a))
        .map(|t| format!("{indent}{t}\n"))
        .collect()
}

fn class_body(dex: &Dex, c: usize, name: &str) -> R<String> {
    let def = dex.class_def(c)?;
    let data = dex.class_data(c)?;
    let anns = dex.annotations(c).unwrap_or_default();
    let mut out = String::new();
    if crate::deobf::class(&def.class).is_some() {
        let orig = def
            .class
            .trim_start_matches('L')
            .trim_end_matches(';')
            .replace('/', ".");
        let _ = writeln!(out, "/* renamed from: {orig} */");
    }
    out += &user_note(&def.class, "");
    out += &annotation_lines(dex, &anns.class, "");
    // a nested class's real modifiers (private, static ...) live in its InnerClass annotation
    let a = system(&anns.class, dex, "Ldalvik/annotation/InnerClass;")
        .and_then(|ic| element(ic, dex, "accessFlags"))
        .and_then(|v| {
            if let dex::Value::Int(f, _) = v {
                Some(*f as u32)
            } else {
                None
            }
        })
        .unwrap_or(def.access);
    let kind = if a & 0x2000 != 0 {
        "@interface"
    } else if a & 0x200 != 0 {
        "interface"
    } else if a & 0x4000 != 0 {
        "enum"
    } else {
        "class"
    };
    // interfaces are implicitly abstract; enums can't be declared final or abstract
    let mods = modifiers(a & if a & 0x4200 != 0 { !0x410 } else { !0 }, false);
    let _ = write!(out, "{mods}{kind} {name}");
    if let Some(s) = def
        .superclass
        .as_deref()
        .filter(|s| *s != "Ljava/lang/Object;" && kind == "class")
    {
        let _ = write!(out, " extends {}", type_ref(s));
    }
    if !def.interfaces.is_empty() && kind != "@interface" {
        let list: Vec<_> = def.interfaces.iter().map(|i| type_ref(i)).collect();
        let _ = write!(
            out,
            " {} {}",
            if kind == "interface" {
                "extends"
            } else {
                "implements"
            },
            list.join(", ")
        );
    }
    out += " {\n";
    out += &members(
        dex,
        c,
        name,
        a,
        &Parts {
            def: &def,
            data: &data,
            anns: &anns,
        },
        None,
    )?;
    out += "}\n";
    Ok(out)
}

/// The parsed parts of one class that `members` renders.
struct Parts<'a> {
    def: &'a dex::ClassDef,
    data: &'a dex::ClassData<'a>,
    anns: &'a dex::Annotations,
}

/// Fields, nested classes and methods of a class, indented one level.
/// `anon` = synthetic capture fields to hide when the class is printed inline (constructor hidden too).
/// `access` = effective class flags (a nested class's come from its InnerClass annotation).
fn members(
    dex: &Dex,
    c: usize,
    name: &str,
    access: u32,
    parts: &Parts,
    anon: Option<&HashSet<u32>>,
) -> R<String> {
    let Parts { def, data, anns } = parts;
    let mut out = String::new();
    // nested classes and methods are rendered first: rendering decides which anonymous classes get
    // inlined, and an interface's static initializer may turn into field initializers
    let mut kids = Vec::new();
    for k in kids_of(dex, c) {
        let kdesc = dex.class_descriptor(k)?;
        let renamed = |d: &str| crate::deobf::class(d).unwrap_or(d).to_string();
        let inner = class_body(dex, k, &inner_name(&renamed(&kdesc), &renamed(&def.class)))
            .unwrap_or_else(|e| format!("/* class {} failed: {e} */\n", jtype(&kdesc)));
        kids.push((kdesc, inner));
    }
    let mut rendered = Vec::new();
    for m in &data.methods {
        let mname = dex
            .method_parts(m.method_idx)
            .map(|p| p.1)
            .unwrap_or_default();
        if anon.is_some() && mname == "<init>" {
            continue;
        }
        // a broken method must not take down the whole class
        let text = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            method(dex, &def.class, name, m, anns)
        })) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => smali_fallback(dex, m, &format!("could not decompile: {e}")),
            Err(_) => smali_fallback(dex, m, "the decompiler crashed on this method"),
        };
        // the compiler writes values()/valueOf()/$values() for every enum
        let synthetic_enum = access & 0x4000 != 0
            && (m.access & 0x1000 != 0
                || ((mname == "values" || mname == "valueOf") && m.access & 0x8 != 0));
        if synthetic_enum {
            continue;
        }
        let text = if access & 0x4000 != 0 && mname == "<init>" {
            enum_ctor(&text)
        } else {
            text
        };
        rendered.push((mname == "<clinit>", text));
    }
    // interfaces can't have `static { }`: `NAME = value;` lines become field initializers
    let mut inits: HashMap<String, String> = HashMap::new();
    if access & 0x200 != 0
        && let Some(k) = rendered.iter().position(|r| r.0)
    {
        let body: Vec<&str> = rendered[k]
            .1
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != "static {" && *l != "}")
            .collect();
        let parsed: Option<Vec<(String, String)>> = body
            .iter()
            .map(|l| {
                let (n, v) = l.strip_suffix(';')?.split_once(" = ")?;
                n.chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                    .then(|| (n.to_string(), v.to_string()))
            })
            .collect();
        if let Some(p) = parsed {
            inits.extend(p);
            rendered.remove(k);
        }
    }
    // enum constants: `A = new Foo("A", 0, x);` in the static initializer -> `A(x)`
    let mut constants: Vec<String> = Vec::new();
    let mut constant_fields = HashSet::new();
    if access & 0x4000 != 0
        && let Some((text, idx)) = enum_constants(dex, data, def, name, &mut rendered)
    {
        constants = text;
        constant_fields = idx;
    }
    if !constants.is_empty() {
        let _ = writeln!(out, "    {};\n", constants.join(",\n    "));
    } else if access & 0x4000 != 0 && anon.is_none() {
        // constants not recognised: an empty constant list keeps the enum body well-formed
        out += "    ;\n";
    }
    // constant initial values of static fields, in static-field order
    let static_vals = dex.static_values(c).unwrap_or_default();
    let mut static_k = 0;
    for &(f, faccess) in &data.fields {
        let static_val = if faccess & 0x8 != 0 {
            static_k += 1;
            static_vals.get(static_k - 1)
        } else {
            None
        };
        if anon.is_some_and(|skip| skip.contains(&f)) {
            continue;
        }
        // enum constants were printed above
        if constant_fields.contains(&f) {
            continue;
        }
        let (_, ty, name) = dex.field_parts(f)?;
        out += &user_note(&format!("{}->{name}", def.class), "    ");
        if let Some(fa) = anns.fields.get(&f) {
            out += &annotation_lines(dex, fa, "    ");
        }
        let (shown, note) = match crate::deobf::field(&def.class, &name, &ty) {
            Some(n) => (n, format!(" // renamed from: {name}")),
            None => (name.clone(), String::new()),
        };
        let init = match (inits.get(&shown), static_val) {
            (Some(v), _) => format!(" = {v}"),
            // the default (0/null/false) is implied; only real constants are written out
            (None, Some(v))
                if !matches!(
                    v,
                    dex::Value::Null | dex::Value::Int(0, _) | dex::Value::Bool(false)
                ) || faccess & 0x10 != 0 =>
            {
                format!(" = {}", typed_value(dex, v, &ty))
            }
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "    {}{} {shown}{init};{note}",
            modifiers(faccess, false),
            type_ref(&ty)
        );
    }
    let methods: String = rendered
        .into_iter()
        .map(|(_, t)| format!("\n{t}"))
        .collect();
    let _ = &static_vals;
    for (kdesc, inner) in kids {
        if INLINED.with(|i| i.borrow().contains(&kdesc)) {
            continue;
        }
        out.push('\n');
        for l in inner.lines() {
            if l.is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(out, "    {l}");
            }
        }
    }
    out += &methods;
    Ok(out)
}

/// (dex address, top-level class, its nested classes with their parents) of the current file
type Family = (usize, usize, Vec<(usize, Option<usize>)>);

thread_local! {
    /// anonymous classes printed inline in the current file
    static INLINED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// capture field -> expression, innermost inline anonymous class last
    static CAPTURES: RefCell<Vec<HashMap<u32, String>>> = const { RefCell::new(Vec::new()) };
    static FAMILY: RefCell<Family> = const { RefCell::new((0, usize::MAX, Vec::new())) };
}

/// Classes directly nested in `c`, in class order (what `nesting().kids[c]` holds), taken from
/// the current file's family so the nesting of every class in the DEX never has to be built.
fn kids_of(dex: &Dex, c: usize) -> Vec<usize> {
    let key = dex as *const Dex as usize;
    let from = |fam: &[(usize, Option<usize>)]| {
        fam.iter().filter(|f| f.1 == Some(c)).map(|f| f.0).collect()
    };
    FAMILY
        .with(|f| {
            let f = f.borrow();
            (f.0 == key && (f.1 == c || f.2.iter().any(|x| x.0 == c))).then(|| from(&f.2))
        })
        .unwrap_or_else(|| {
            from(
                &dex.descendants(c)
                    .into_iter()
                    .map(|k| (k, dex.parent_of(k)))
                    .collect::<Vec<_>>(),
            )
        })
}

/// Text standing in for a synthetic capture field (`this.val$x` -> `x`) inside an inline class.
pub fn capture(field: u32) -> Option<String> {
    CAPTURES.with(|c| c.borrow().last().and_then(|m| m.get(&field).cloned()))
}

/// What a javac anonymous-class constructor does: capture fields from params, then `super(...)`.
struct AnonCtor {
    /// param index -> capture field
    fields: Vec<(usize, u32)>,
    /// super constructor args: param index or literal
    super_args: Vec<Result<usize, ir::Expr>>,
    super_ctor: u32,
}

fn anon_ctor(dex: &Dex, c: usize) -> Option<AnonCtor> {
    let anns = dex.annotations(c).ok()?;
    // anonymous: InnerClass annotation without a name
    let ic = system(&anns.class, dex, "Ldalvik/annotation/InnerClass;")?;
    if !matches!(element(ic, dex, "name"), Some(dex::Value::Null)) {
        return None;
    }
    let data = dex.class_data(c).ok()?;
    let mut ctors = data.methods.iter().filter(|m| {
        dex.method_parts(m.method_idx)
            .is_ok_and(|p| p.1 == "<init>")
    });
    let (ctor, None) = (ctors.next()?, ctors.next()) else {
        return None;
    };
    let code = ctor.code.as_ref()?;
    let insns = bytecode::decode(code.insns).ok()?;
    let cfg = cfg::build(&insns, code.insns, &code.tries).ok()?;
    if cfg.blocks.len() != 1 {
        return None;
    }
    let mut bir: Bir = vec![
        ir::lower(
            dex,
            &insns,
            code.insns,
            0,
            cfg.blocks[0].last,
            &cfg.blocks[0].term,
        )
        .ok()?,
    ];
    opt::inline(&cfg, &mut bir, code.registers as usize);
    let (_, _, proto) = dex.method_parts(ctor.method_idx).ok()?;
    let params = dex.proto_parts(proto).ok()?.1;
    let this = code.registers - code.ins;
    let mut reg_param = HashMap::new();
    let mut r = this + 1;
    for (k, p) in params.iter().enumerate() {
        reg_param.insert(r, k);
        r += if p == "J" || p == "D" { 2 } else { 1 };
    }
    let mut out = AnonCtor {
        fields: vec![],
        super_args: vec![],
        super_ctor: u32::MAX,
    };
    for st in &bir[0].0 {
        match st {
            ir::Stmt::Store(ir::Expr::Field(Some(o), f), ir::Expr::Reg(p))
                if **o == ir::Expr::Reg(this) =>
            {
                out.fields.push((*reg_param.get(p)?, *f))
            }
            ir::Stmt::Do(ir::Expr::Invoke(ir::Call::Direct, m, a))
                if a.first() == Some(&ir::Expr::Reg(this)) && out.super_ctor == u32::MAX =>
            {
                out.super_ctor = *m;
                for x in &a[1..] {
                    out.super_args.push(match x {
                        ir::Expr::Reg(p) => Ok(*reg_param.get(p)?),
                        l @ ir::Expr::Lit(..) => Err(l.clone()),
                        _ => return None,
                    });
                }
            }
            ir::Stmt::Return(None) => {}
            _ => return None,
        }
    }
    (out.super_ctor != u32::MAX).then_some(out)
}

/// `new Runnable() { ... }` for an anonymous class created at this site, when that is exact.
pub fn anon_new(ctx: &ir::Ctx, desc: &str, args: &[ir::Expr]) -> Option<String> {
    let dex = ctx.dex;
    if CAPTURES.with(|c| c.borrow().len()) > 8 || !args.iter().all(ir::Expr::is_pure) {
        return None;
    }
    let c = dex.find_class(desc)?;
    let info = anon_ctor(dex, c)?;
    let def = dex.class_def(c).ok()?;
    let sup = match def.superclass.as_deref() {
        Some("Ljava/lang/Object;") | None => def
            .interfaces
            .first()
            .cloned()
            .unwrap_or_else(|| "Ljava/lang/Object;".into()),
        Some(s) => s.to_string(),
    };
    let sparams = dex
        .method_parts(info.super_ctor)
        .ok()
        .and_then(|(_, _, p)| dex.proto_parts(p).ok())
        .map(|p| p.1)
        .unwrap_or_default();
    let mut sargs = Vec::new();
    for (k, a) in info.super_args.iter().enumerate() {
        let want = sparams.get(k).map(String::as_str);
        sargs.push(match a {
            Ok(p) => ctx.expr(args.get(*p)?, want),
            Err(lit) => ctx.expr(lit, want),
        });
    }
    let outer_this = format!("{}.this", ir::simple(ctx.class));
    let mut caps = HashMap::new();
    for &(p, f) in &info.fields {
        let text = ctx.expr(args.get(p)?, None);
        caps.insert(
            f,
            if text == "this" {
                outer_this.clone()
            } else {
                text
            },
        );
    }
    let data = dex.class_data(c).ok()?;
    let anns = dex.annotations(c).ok()?;
    let hidden: HashSet<u32> = caps.keys().copied().collect();
    CAPTURES.with(|x| x.borrow_mut().push(caps));
    let body = members(
        dex,
        c,
        &ir::simple(&sup),
        def.access,
        &Parts {
            def: &def,
            data: &data,
            anns: &anns,
        },
        Some(&hidden),
    );
    CAPTURES.with(|x| x.borrow_mut().pop());
    let body = body.ok()?;
    INLINED.with(|i| i.borrow_mut().insert(def.class.clone()));
    Some(format!(
        "new {}({}) {{\n{}}}",
        type_ref(&sup),
        sargs.join(", "),
        body.trim_start_matches('\n')
    ))
}

fn method(
    dex: &Dex,
    class: &str,
    class_name: &str,
    m: &dex::Method,
    anns: &dex::Annotations,
) -> R<String> {
    let (owner, mut name, proto) = dex.method_parts(m.method_idx)?;
    let renamed_from = crate::deobf::method(&owner, &name).map(|n| std::mem::replace(&mut name, n));
    let (ret, params) = dex.proto_parts(proto)?;
    let is_static = m.access & 0x8 != 0;
    let mods = modifiers(m.access, true);
    let no_anns = Vec::new();
    let manns = anns.methods.get(&m.method_idx).unwrap_or(&no_anns);
    let note = user_note(
        &format!("{owner}->{}()", renamed_from.as_deref().unwrap_or(&name)),
        "    ",
    );
    let pre = format!(
        "{note}{}{}",
        renamed_comment(&renamed_from),
        annotation_lines(dex, manns, "    ")
    );
    let throws = system(manns, dex, "Ldalvik/annotation/Throws;")
        .and_then(|t| element(t, dex, "value"))
        .map_or(String::new(), |v| match v {
            dex::Value::Array(ts) if !ts.is_empty() => {
                format!(
                    " throws {}",
                    ts.iter()
                        .map(|t| value_text(dex, t).trim_end_matches(".class").to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            _ => String::new(),
        });
    let pann = |k: usize| -> String {
        anns.params
            .get(&m.method_idx)
            .and_then(|p| p.get(k))
            .map_or(String::new(), |set| {
                set.iter()
                    .filter_map(|a| annotation_text(dex, a))
                    .map(|t| t + " ")
                    .collect()
            })
    };
    let Some(code) = &m.code else {
        let mut used = HashMap::new();
        let sig: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let base = ir::name_for(Some(p));
                let n = used.entry(base.clone()).or_insert(0);
                *n += 1;
                format!(
                    "{}{} {}",
                    pann(k),
                    type_ref(p),
                    if *n == 1 { base } else { format!("{base}{n}") }
                )
            })
            .collect();
        let default = system(&anns.class, dex, "Ldalvik/annotation/AnnotationDefault;")
            .and_then(|d| element(d, dex, "value"))
            .and_then(|v| {
                if let dex::Value::Annotation(a) = v {
                    element(a, dex, &name).map(|x| format!(" default {}", value_text(dex, x)))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let head = format!(
            "{mods}{} {name}({}){throws}{default}",
            type_ref(&ret),
            sig.join(", ")
        );
        return Ok(format!("{pre}    {head};\n"));
    };

    let insns = bytecode::decode(code.insns)?;
    let mut cfg = cfg::build(&insns, code.insns, &code.tries)?;
    let mut bir: Bir = cfg
        .blocks
        .iter()
        .map(|b| ir::lower(dex, &insns, code.insns, b.first, b.last, &b.term))
        .collect::<R<_>>()?;
    opt::inline(&cfg, &mut bir, code.registers as usize);
    if opt::merge_conditions(&mut cfg, &mut bir) {
        cfg.recompute();
    }

    // incoming registers: `this` then parameters (wide ones take two)
    let mut param_regs = Vec::new();
    let mut param_info = Vec::new(); // (descriptor, fixed name)
    let mut r = code.registers - code.ins;
    if !is_static {
        param_regs.push(r);
        param_info.push((class.to_string(), Some("this".to_string())));
        r += 1;
    }
    for p in &params {
        param_regs.push(r);
        param_info.push((p.clone(), None));
        r += if p == "J" || p == "D" { 2 } else { 1 };
    }
    let mut ctx = Ctx {
        dex,
        class,
        ret: ret.clone(),
        vars: None,
    };
    if let Some(vars) = opt::webs(&cfg, &mut bir, &param_regs) {
        ir::fold_split_new(dex, &cfg, &mut bir, &vars);
        ir::propagate_constants(&mut bir, &vars);
        ir::fold_concat(dex, &mut bir, &vars);
        let ps: Vec<(usize, String, Option<String>)> = param_info
            .iter()
            .cloned()
            .enumerate()
            .map(|(d, (t, n))| (d, t, n))
            .collect();
        ctx.assign_vars(vars, &bir, &ps);
    }
    let pname = |k: usize| -> String {
        let d = k + !is_static as usize;
        ctx.vars
            .as_ref()
            .map_or(format!("p{k}"), |v| v.name[v.root[d]].clone())
    };
    let sig: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(k, p)| format!("{}{} {}", pann(k), type_ref(p), pname(k)))
        .collect();
    let head = match name.as_str() {
        "<clinit>" => "static".to_string(),
        "<init>" => format!("{mods}{}({}){throws}", class_name, sig.join(", ")),
        _ => format!(
            "{mods}{} {name}({}){throws}",
            type_ref(&ret),
            sig.join(", ")
        ),
    };

    let mut e = Emitter::new(&ctx, &insns, &cfg, &bir);
    // variables assigned in several places are declared up front
    if let Some(v) = &ctx.vars {
        for r in 0..v.root.len() {
            if v.root[r] == r && v.defs[r] > 1 && !v.param[r] {
                let t = v.ty[r].as_deref().map_or("Object".into(), type_ref);
                e.line(2, format!("{t} {};", v.name[r]));
            }
        }
    }
    e.seq(0, None, 2);
    // exception handlers not reached by the structured walk
    let mut handlers: Vec<(Option<u32>, usize)> = cfg
        .blocks
        .iter()
        .flat_map(|b| b.handlers.iter().copied())
        .collect();
    handlers.sort_by_key(|h| h.1);
    handlers.dedup_by_key(|h| h.1);
    for (ty, h) in handlers {
        if !e.visited[h] {
            let ty = ty.map_or(Ok("Throwable".into()), |t| {
                dex.type_name(t).map(|t| type_ref(&t))
            })?;
            e.line(1, format!("// catch ({ty}) handler"));
            e.seq(h, None, 2);
        }
    }
    let body = e.finish();
    let warn = warnings(&body);
    let warn = if warn.is_empty() {
        String::new()
    } else {
        format!(
            "    /* WARNING: {}. The Java below may not match the bytecode exactly; check the Smali. */\n",
            warn.join("; ")
        )
    };
    Ok(format!("{pre}{warn}    {head} {{\n{body}    }}\n"))
}

/// `T x = v;` declared inside a block but read after the block closes is not valid Java:
/// move the declaration to the top of the method and keep a plain assignment in place.
fn hoist_declarations(lines: &mut Vec<(usize, String)>) {
    let is_label = |l: &str| {
        l.ends_with(':')
            && l.starts_with('L')
            && l[1..l.len() - 1].chars().all(|c| c.is_ascii_hexdigit())
    };
    // word -> line numbers it appears on
    let mut seen: HashMap<&str, Vec<usize>> = HashMap::new();
    for (k, (_, l)) in lines.iter().enumerate() {
        for w in l
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .filter(|w| !w.is_empty())
        {
            let v = seen.entry(w).or_default();
            if v.last() != Some(&k) {
                v.push(k);
            }
        }
    }
    let mut hoisted: Vec<(usize, String, String)> = Vec::new();
    for (k, (d, l)) in lines.iter().enumerate() {
        let Some((ty, rest)) = l.split_once(' ') else {
            continue;
        };
        let Some((name, _)) = rest.split_once(" = ") else {
            continue;
        };
        // class types are still import markers (\u{1}a.b.C\u{2}) at this point
        let typed = ty
            .chars()
            .all(|c| c.is_alphanumeric() || "_$./[]\u{1}\u{2}".contains(c))
            && ty
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '\u{1}');
        if !typed
            || name.contains(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            || ["return", "throw", "new", "else"].contains(&ty)
        {
            continue;
        }
        // the declaring block ends at the first later line that is less indented (labels don't close blocks)
        let end = lines[k + 1..]
            .iter()
            .position(|(i, t)| i < d && !is_label(t))
            .map(|p| k + 1 + p);
        if let Some(end) = end
            && seen.get(name).is_some_and(|v| v.iter().any(|&u| u >= end))
        {
            hoisted.push((k, ty.to_string(), name.to_string()));
        }
    }
    for (k, ty, name) in hoisted.iter().rev() {
        let l = &mut lines[*k].1;
        *l = l[ty.len() + 1..].to_string();
        let _ = name;
    }
    // a name declared in two arms is hoisted once
    let mut top: Vec<(usize, String)> = hoisted
        .into_iter()
        .map(|(_, ty, name)| (2, format!("{ty} {name};")))
        .collect();
    let mut once = std::collections::HashSet::new();
    top.retain(|l| once.insert(l.1.clone()));
    lines.splice(0..0, top);
}

/// Split `a, f(b, c), "x, y"` at top-level commas.
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_q), '\\') => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                continue;
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Enum constants from the static initializer: static fields of the enum's own type assigned
/// `new Enum("NAME", ordinal, ...)` (directly or through a temporary). Those lines leave the
/// initializer; temporaries left in it are renamed to their constant. None unless the ordinals
/// are exactly 0..n.
fn enum_constants(
    dex: &Dex,
    data: &dex::ClassData,
    def: &dex::ClassDef,
    name: &str,
    rendered: &mut Vec<(bool, String)>,
) -> Option<(Vec<String>, HashSet<u32>)> {
    let k = rendered.iter().position(|r| r.0)?;
    // shown name -> field idx, for static fields typed as this enum
    let mut own: HashMap<String, u32> = HashMap::new();
    for &(f, access) in &data.fields {
        let (_, ty, n) = dex.field_parts(f).ok()?;
        if access & 0x8 != 0 && ty == def.class {
            own.insert(crate::deobf::field(&def.class, &n, &ty).unwrap_or(n), f);
        }
    }
    // type names are still import markers at this point
    let prefix = format!("new {}(", type_ref(&def.class));
    let _ = name;
    let mut temps: HashMap<String, Vec<String>> = HashMap::new(); // temp var -> ctor args
    let mut found: Vec<(i64, String, String, u32)> = Vec::new(); // (ordinal, field, constant text, idx)
    let mut alias: HashMap<String, String> = HashMap::new(); // temp var -> field
    let mut copy_of: HashMap<String, String> = HashMap::new(); // copy of a temp -> that temp
    // `T v = Other.FIELD;` / literals: the constant's arguments can't name a static-block local
    let mut locals: HashMap<String, String> = HashMap::new();
    let simple = |e: &str| {
        (e.len() > 1 && e.starts_with('"') && e.ends_with('"'))
            || e.chars()
                .all(|c| c.is_alphanumeric() || "_$./-\u{1}\u{2}".contains(c))
    };
    let mut keep = Vec::new();
    for line in rendered[k].1.lines() {
        let t = line.trim();
        let assign = t.strip_suffix(';').and_then(|x| x.split_once(" = "));
        let mut consumed = false;
        if let Some((lhs, rhs)) = assign {
            let lhs_var = lhs.rsplit(' ').next().unwrap_or(lhs);
            let ctor = rhs
                .strip_prefix(&prefix)
                .and_then(|a| a.strip_suffix(')'))
                .map(split_args)
                .map(|a| {
                    a.into_iter()
                        .map(|x| locals.get(&x).cloned().unwrap_or(x))
                        .collect::<Vec<_>>()
                });
            match (own.get(lhs), ctor) {
                (Some(&f), Some(args)) => {
                    if let Some(c) = constant(lhs, &args) {
                        found.push((c.0, lhs.to_string(), c.1, f));
                        consumed = true;
                    }
                }
                (None, Some(args)) if lhs.contains(' ') => {
                    temps.insert(lhs_var.to_string(), args);
                    consumed = true;
                }
                (Some(&f), None) => {
                    let tmp = copy_of.get(rhs).map_or(rhs, String::as_str);
                    // a second field holding an existing constant stays a plain assignment
                    if let Some(args) = temps.get(tmp).filter(|_| !alias.contains_key(tmp))
                        && let Some(c) = constant(lhs, args)
                    {
                        found.push((c.0, lhs.to_string(), c.1, f));
                        for (cp, _) in copy_of.iter().filter(|(_, t)| t.as_str() == tmp) {
                            alias.insert(cp.clone(), lhs.to_string());
                        }
                        alias.insert(tmp.to_string(), lhs.to_string());
                        consumed = true;
                    }
                }
                // `T b = a;` copying a temporary already stored to its field: b names that constant too
                (None, None) if lhs.contains(' ') && alias.contains_key(rhs) => {
                    let field = alias[rhs].clone();
                    alias.insert(lhs_var.to_string(), field);
                    consumed = true;
                }
                // ... or one not stored yet: the field may be assigned from the copy
                (None, None) if lhs.contains(' ') && temps.contains_key(rhs) => {
                    copy_of.insert(lhs_var.to_string(), rhs.to_string());
                    consumed = true;
                }
                (None, None) if lhs.contains(' ') && simple(rhs) => {
                    locals.insert(
                        lhs_var.to_string(),
                        locals.get(rhs).cloned().unwrap_or(rhs.to_string()),
                    );
                }
                _ => {}
            }
        }
        if !consumed {
            keep.push(line.to_string());
        }
    }
    // R8 drops the fields of constants nobody reads: name those from their string
    let mut unnamed: Vec<(&String, &Vec<String>)> = temps
        .iter()
        .filter(|(t, _)| !alias.contains_key(*t))
        .collect();
    unnamed.sort();
    for (tmp, args) in unnamed {
        let name = args
            .first()
            .map(|a| a.trim_matches('"').to_string())
            .unwrap_or_default();
        let ident = name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident || own.contains_key(&name) || found.iter().any(|c| c.1 == name) {
            continue;
        }
        let Some(c) = constant(&name, args) else {
            continue;
        };
        found.push((c.0, name.clone(), c.1, u32::MAX));
        for (cp, _) in copy_of.iter().filter(|(_, t)| *t == tmp) {
            alias.insert(cp.clone(), name.clone());
        }
        alias.insert(tmp.clone(), name);
    }
    found.sort();
    if found.is_empty()
        || found.iter().enumerate().any(|(i, c)| c.0 != i as i64)
        || !temps
            .keys()
            .chain(copy_of.keys())
            .all(|t| alias.contains_key(t))
    {
        return None;
    }
    // temporaries still referenced (e.g. by the values array) now name their constant
    let keep: Vec<String> = keep
        .into_iter()
        .map(|l| {
            let mut l = l;
            for (tmp, field) in &alias {
                l = replace_word(&l, tmp, field);
            }
            l
        })
        .collect();
    let body: Vec<&String> = keep
        .iter()
        .filter(|l| !matches!(l.trim(), "static {" | "}" | ""))
        .collect();
    if body.is_empty() {
        rendered.remove(k);
    } else {
        rendered[k].1 = keep.join("\n") + "\n";
    }
    let idx = found
        .iter()
        .map(|c| c.3)
        .filter(|&f| f != u32::MAX)
        .collect();
    Some((found.into_iter().map(|c| c.2).collect(), idx))
}

/// (ordinal, `NAME(rest)`) from `("NAME", ordinal, rest...)`
fn constant(field: &str, args: &[String]) -> Option<(i64, String)> {
    let ord: i64 = args.get(1)?.parse().ok()?;
    if !args.first()?.starts_with('"') {
        return None;
    }
    let rest = args.get(2..).unwrap_or(&[]).join(", ");
    Some((
        ord,
        if rest.is_empty() {
            field.to_string()
        } else {
            format!("{field}({rest})")
        },
    ))
}

fn replace_word(s: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    while i < s.len() {
        if s[i..].starts_with(from)
            && (i == 0 || !word(b[i - 1]))
            && b.get(i + from.len()).is_none_or(|&c| !word(c))
        {
            out += to;
            i += from.len();
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Enum constructors take hidden (name, ordinal) first and pass them to `super`: drop both.
fn enum_ctor(text: &str) -> String {
    let mut out = Vec::new();
    let mut head_done = false;
    for line in text.lines() {
        let t = line.trim();
        if !head_done && t.ends_with('{') && t.contains('(') {
            head_done = true;
            if let (Some(i), Some(j)) = (line.find('('), line.rfind(')')) {
                let params = split_args(&line[i + 1..j]);
                // enum constructors are implicitly private; any access modifier is an error
                let head = line[..i]
                    .replacen("public ", "", 1)
                    .replacen("protected ", "", 1)
                    .replacen("private ", "", 1);
                out.push(format!(
                    "{head}({}){}",
                    params.get(2..).unwrap_or(&[]).join(", "),
                    &line[j + 1..]
                ));
                continue;
            }
        }
        if head_done && t.starts_with("super(") {
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

/// Why a structured body might mislead, judged from what the printer had to fall back to.
pub fn warnings(body: &str) -> Vec<String> {
    let mut w = Vec::new();
    let gotos = body.matches("goto L").count();
    if gotos > 0 {
        w.push(format!(
            "{gotos} jump{} could not be structured",
            if gotos == 1 { "" } else { "s" }
        ));
    }
    if body.contains("// catch (") {
        w.push("exception handler shown outside its try block".into());
    }
    if body.contains("monitorEnter(") {
        w.push("lock not matched to a synchronized block".into());
    }
    if body.contains("<result>") || body.contains("if (?)") || body.contains("switch (?)") {
        w.push("a value could not be recovered".into());
    }
    w
}

/// The method's Dalvik bytecode as a comment: shown instead of Java when decompiling fails.
pub fn smali_fallback(dex: &Dex, m: &dex::Method, why: &str) -> String {
    let mut out = format!(
        "    /* WARNING: {why}. Showing Dalvik bytecode instead.\n     * {}\n",
        dex.method_ref(m.method_idx).unwrap_or_default()
    );
    if let Some(code) = &m.code {
        match bytecode::decode(code.insns) {
            Ok(insns) => {
                for i in insns {
                    let _ = writeln!(
                        out,
                        "     *   {:04x}: {}",
                        i.off,
                        crate::render(dex, &i)
                            .unwrap_or_else(|e| e.to_string())
                            .replace("*/", "* /")
                    );
                }
            }
            Err(e) => {
                let _ = writeln!(out, "     *   (bytecode unreadable: {e})");
            }
        }
    }
    out + "     */\n"
}

/// The user's comment from the project file, as `// NOTE:` lines.
fn user_note(key: &str, indent: &str) -> String {
    crate::deobf::comment(key).map_or(String::new(), |c| {
        c.lines()
            .map(|l| {
                format!(
                    "{indent}// NOTE: {l}
"
                )
            })
            .collect()
    })
}

fn renamed_comment(from: &Option<String>) -> String {
    from.as_ref().map_or(String::new(), |f| {
        format!(
            "    /* renamed from: {f} */
"
        )
    })
}

/// For each block: the closest (reverse-postorder) block it immediately dominates that has two or
/// more forward predecessors. Used as an `if` join when no post-dominator exists.
fn merge_points(cfg: &Cfg, header: &[bool]) -> Vec<Option<usize>> {
    let n = cfg.blocks.len();
    let succ: Vec<Vec<usize>> = cfg
        .blocks
        .iter()
        .map(|b| {
            b.succs()
                .into_iter()
                .chain(b.handlers.iter().map(|h| h.1))
                .collect()
        })
        .collect();
    let mut rpo = vec![usize::MAX; n];
    let (mut seen, mut order, mut stack) = (
        vec![false; n],
        Vec::with_capacity(n),
        vec![(0usize, 0usize)],
    );
    seen[0] = true;
    while let Some((v, i)) = stack.pop() {
        if let Some(&c) = succ[v].get(i) {
            stack.push((v, i + 1));
            if !seen[c] {
                seen[c] = true;
                stack.push((c, 0));
            }
        } else {
            order.push(v);
        }
    }
    for (k, &v) in order.iter().rev().enumerate() {
        rpo[v] = k;
    }
    let mut fwd_preds = vec![0usize; n];
    for (b, blk) in cfg.blocks.iter().enumerate() {
        for s in blk.succs() {
            if !cfg.dominates(s, b) {
                fwd_preds[s] += 1;
            }
        }
    }
    let mut best: Vec<Option<usize>> = vec![None; n];
    for k in 0..n {
        let d = cfg.idom[k];
        if d == k || fwd_preds[k] < 2 || header[k] || rpo[k] == usize::MAX {
            continue;
        }
        if best[d].is_none_or(|x| rpo[k] < rpo[x]) {
            best[d] = Some(k);
        }
    }
    best
}

struct Emitter<'a> {
    ctx: &'a Ctx<'a>,
    insns: &'a [Insn],
    cfg: &'a Cfg,
    bir: &'a Bir,
    visited: Vec<bool>,
    header: Vec<bool>,
    follow: Vec<Option<usize>>,
    /// (header, follow) of loops being emitted, innermost last
    loops: Vec<(usize, Option<usize>)>,
    lines: Vec<(usize, String)>,
    /// fallback join of an `if` whose arms don't share a post-dominator (one arm returns early):
    /// the nearest block it dominates that two paths merge into
    merge: Vec<Option<usize>>,
    /// first output line of each emitted block
    starts: HashMap<usize, usize>,
    labels: BTreeSet<usize>,
    /// handler lists of the try blocks being emitted, innermost last
    trys: Vec<Vec<(Option<u32>, usize)>>,
    /// first block reached that is outside the innermost try
    try_exit: Option<usize>,
    /// small regions being repeated inside each other; flattened control flow would repeat forever
    dups: usize,
    /// leading statements to skip per block (catch parameter, code hoisted into `finally`)
    skip: HashMap<usize, usize>,
}

const MAX_DEPTH: usize = 200;

impl<'a> Emitter<'a> {
    fn new(ctx: &'a Ctx<'a>, insns: &'a [Insn], cfg: &'a Cfg, bir: &'a Bir) -> Self {
        let n = cfg.blocks.len();
        let mut header = vec![false; n];
        let mut body: Vec<Vec<usize>> = vec![vec![]; n];
        let mut preds = vec![vec![]; n];
        for (b, blk) in cfg.blocks.iter().enumerate() {
            for s in blk.succs() {
                preds[s].push(b);
            }
        }
        for (b, blk) in cfg.blocks.iter().enumerate() {
            for h in blk.succs() {
                if cfg.dominates(h, b) {
                    header[h] = true;
                    // natural loop: walk back from latch to header
                    let mut stack = vec![b];
                    while let Some(x) = stack.pop() {
                        if !body[h].contains(&x) && x != h {
                            body[h].push(x);
                            stack.extend(&preds[x]);
                        }
                    }
                }
            }
        }
        let follow = (0..n)
            .map(|h| {
                let inside = |x: usize| x == h || body[h].contains(&x);
                body[h]
                    .iter()
                    .chain([&h])
                    .flat_map(|&x| cfg.blocks[x].succs())
                    .filter(|&s| !inside(s))
                    .min()
            })
            .collect();
        let follow = Self::loop_follows(cfg, &body, follow);
        Emitter {
            merge: merge_points(cfg, &header),
            ctx,
            insns,
            cfg,
            bir,
            visited: vec![false; n],
            header,
            follow,
            loops: vec![],
            lines: vec![],
            starts: HashMap::new(),
            labels: BTreeSet::new(),
            trys: vec![],
            try_exit: None,
            dups: 0,
            skip: HashMap::new(),
        }
    }

    /// A loop with several exits that merge again (`found -> x = 1`, `done -> x = 0`) follows at the
    /// merge, so each exit becomes `...; break;` instead of one exit running on inside the loop.
    fn loop_follows(
        cfg: &Cfg,
        body: &[Vec<usize>],
        mut follow: Vec<Option<usize>>,
    ) -> Vec<Option<usize>> {
        let chain = |mut x: usize| {
            let mut c = vec![x];
            while let Some(p) = cfg.ipdom[x].filter(|&p| p != x && c.len() < 64) {
                c.push(p);
                x = p;
            }
            c
        };
        for h in 0..body.len() {
            let Some(first) = follow[h] else { continue };
            let inside = |x: usize| x == h || body[h].contains(&x);
            let mut exits: Vec<usize> = body[h]
                .iter()
                .chain([&h])
                .flat_map(|&x| cfg.blocks[x].succs())
                .filter(|&s| !inside(s))
                .collect();
            exits.sort_unstable();
            exits.dedup();
            // exits that leave the method (return/throw) don't need to meet the others
            let chains: Vec<Vec<usize>> = exits
                .iter()
                .map(|&e| chain(e))
                .filter(|c| c.len() > 1 || cfg.blocks[c[0]].term != Term::Exit)
                .collect();
            if chains.len() < 2 {
                continue;
            }
            // the merge must stay inside every enclosing loop and the same try
            let ok = |m: usize| {
                !inside(m)
                    && cfg.blocks[m].handlers == cfg.blocks[h].handlers
                    && (0..body.len())
                        .all(|h2| h2 == h || !body[h2].contains(&h) || body[h2].contains(&m))
            };
            if let Some(m) = chain(first)
                .into_iter()
                .find(|&m| ok(m) && chains.iter().all(|c| c.contains(&m)))
            {
                follow[h] = Some(m);
            }
        }
        follow
    }

    fn line(&mut self, ind: usize, s: String) {
        self.lines.push((ind, s));
    }

    fn label(&self, b: usize) -> String {
        format!("L{:x}", self.insns[self.cfg.blocks[b].first].off)
    }

    /// Drop an emitted line, keeping recorded block positions valid.
    fn remove_line(&mut self, k: usize) {
        self.lines.remove(k);
        for s in self.starts.values_mut() {
            if *s > k {
                *s -= 1;
            }
        }
    }

    fn finish(mut self) -> String {
        for &b in self.labels.iter().rev() {
            if let Some(&at) = self.starts.get(&b) {
                let ind = self.lines[at].0.saturating_sub(1);
                let l = format!("{}:", self.label(b));
                self.lines.insert(at, (ind, l));
            }
        }
        // implicit `return;` at the end of a void method
        if self
            .lines
            .last()
            .is_some_and(|l| l.0 == 2 && l.1 == "return;")
            && !self
                .lines
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|l| l.1.ends_with(':'))
        {
            self.lines.pop();
        }
        hoist_declarations(&mut self.lines);
        let mut s = String::new();
        for (ind, l) in self.lines {
            let pad = "    ".repeat(ind);
            for part in l.split('\n') {
                let _ = writeln!(s, "{pad}{part}");
            }
        }
        s
    }

    /// break/continue when `t` leaves or restarts an enclosing loop
    fn jump(&mut self, t: usize) -> Option<String> {
        let depth = self.loops.len();
        for (k, &(h, f)) in self.loops.iter().enumerate().rev() {
            let inner = k + 1 == depth;
            let kw = if t == h {
                "continue"
            } else if f == Some(t) {
                "break"
            } else {
                continue;
            };
            if inner {
                return Some(format!("{kw};"));
            }
            self.labels.insert(h);
            return Some(format!("{kw} {};", self.label(h)));
        }
        None
    }

    fn jump_target(&self, t: usize) -> Option<()> {
        self.loops
            .iter()
            .any(|&(h, f)| t == h || f == Some(t))
            .then_some(())
    }

    /// `try { ... } catch (T e) { ... }` starting at `b`; returns where flow continues.
    fn try_block(&mut self, b: usize, ind: usize) -> Option<usize> {
        let hs = self.cfg.blocks[b].handlers.clone();
        let outer = self.trys.last().map_or(0, |t| t.len());
        if outer == 0
            && let Some(r) = self.synchronized(b, &hs, ind)
        {
            return r;
        }
        self.starts.entry(b).or_insert(self.lines.len());
        let try_at = self.lines.len();
        self.line(ind, "try {".into());
        self.trys.push(hs.clone());
        let saved = self.try_exit.take();
        self.seq(b, None, ind + 1);
        let exit = std::mem::replace(&mut self.try_exit, saved);
        self.trys.pop();
        let own = &hs[..hs.len() - outer];
        if let (Some(fin), Some(x)) = (self.finally_body(own, exit), exit) {
            // `monitorEnter(o); try { S } finally { monitorExit(o); }` is how d8 writes `synchronized (o) { S }`
            let lock = fin
                .first()
                .and_then(|l| l.strip_prefix("monitorExit("))
                .and_then(|l| l.strip_suffix(");"))
                .map(str::to_string);
            if let (Some(o), 1, 1) = (lock, fin.len(), own.len()) {
                let enter = format!("monitorEnter({o});");
                if try_at > 0 && self.lines[try_at - 1] == (ind, enter) {
                    self.remove_line(try_at - 1);
                    self.lines[try_at - 1].1 = format!("synchronized ({o}) {{");
                    let unlock = format!("monitorExit({o});");
                    let mut k = try_at;
                    while k < self.lines.len() {
                        if self.lines[k].1 == unlock {
                            self.remove_line(k)
                        } else {
                            k += 1
                        }
                    }
                    self.visited[own[0].1] = true;
                    *self.skip.entry(x).or_default() += 1;
                    self.line(ind, "}".into());
                    return exit;
                }
            }
            // catch-all that runs the same code as the normal exit, then rethrows
            for &(ty, h) in &own[..own.len() - 1] {
                self.catch_clause(ty, h, exit, ind);
            }
            self.line(ind, "} finally {".into());
            for l in &fin {
                self.line(ind + 1, l.clone());
            }
            self.visited[own[own.len() - 1].1] = true;
            *self.skip.entry(x).or_default() += fin.len();
            self.line(ind, "}".into());
            return exit;
        }
        for &(ty, h) in own {
            self.catch_clause(ty, h, exit, ind);
        }
        self.line(ind, "}".into());
        exit
    }

    /// `monitorEnter(x); try { S } catch-all { monitorExit(x); throw }` -> `synchronized (x) { S }`.
    /// Returns None when the pattern does not match (nothing emitted then).
    fn synchronized(
        &mut self,
        b: usize,
        hs: &[(Option<u32>, usize)],
        ind: usize,
    ) -> Option<Option<usize>> {
        let &[(None, h)] = hs else { return None };
        let blk = &self.cfg.blocks[h];
        // javac: `e = caught; monitorExit(x)` in one block covered by its own catch-all;
        // d8 splits it: `e = caught` -> `monitorExit(x)` (covered by the catch-all) -> `throw e`
        let (e, x, m, t) = match (self.bir[h].0.as_slice(), &blk.term) {
            (
                [
                    ir::Stmt::Assign(Expr::Var(e), Expr::Caught),
                    ir::Stmt::Monitor(false, x),
                ],
                &Term::Goto(t),
            ) if blk.handlers.iter().any(|&(ty, hh)| ty.is_none() && hh == h) => (e, x, h, t),
            ([ir::Stmt::Assign(Expr::Var(e), Expr::Caught)], &Term::Goto(m)) => {
                let mb = &self.cfg.blocks[m];
                let ([ir::Stmt::Monitor(false, x)], Term::Goto(t)) =
                    (self.bir[m].0.as_slice(), &mb.term)
                else {
                    return None;
                };
                if !mb
                    .handlers
                    .iter()
                    .any(|&(ty, hh)| ty.is_none() && (hh == h || hh == m))
                {
                    return None;
                }
                (e, x, m, *t)
            }
            _ => return None,
        };
        let v = self.ctx.vars.as_ref()?;
        let [ir::Stmt::Throw(Expr::Var(e2))] = self.bir[t].0.as_slice() else {
            return None;
        };
        if v.root[*e] != v.root[*e2] {
            return None;
        }
        let x = self.ctx.expr(x, None);
        let (enter, exit_line) = (format!("monitorEnter({x});"), format!("monitorExit({x});"));
        if self.lines.last().map(|l| &l.1) != Some(&enter) {
            return None;
        }
        self.lines.pop();
        self.starts.entry(b).or_insert(self.lines.len());
        let at = self.lines.len();
        self.line(ind, format!("synchronized ({x}) {{"));
        self.visited[h] = true;
        self.visited[m] = true;
        self.visited[t] = true;
        self.trys.push(hs.to_vec());
        let saved = self.try_exit.take();
        self.seq(b, None, ind + 1);
        let exit = std::mem::replace(&mut self.try_exit, saved);
        self.trys.pop();
        // the unlocks are implied by the block; drop them, keeping label positions valid
        let mut k = at;
        while k < self.lines.len() {
            if self.lines[k].1 == exit_line {
                self.lines.remove(k);
                for s in self.starts.values_mut() {
                    if *s > k {
                        *s -= 1;
                    }
                }
            } else {
                k += 1;
            }
        }
        self.line(ind, "}".into());
        if let Some(x2) = exit
            && self.bir[x2]
                .0
                .first()
                .is_some_and(|s| self.ctx.stmt(s) == exit_line)
        {
            *self.skip.entry(x2).or_default() += 1;
        }
        Some(exit)
    }

    /// Rendered statements of a `finally`: the last handler is a catch-all block
    /// `e = caught; S...; throw e;` and the try's exit starts with the same `S...`.
    fn finally_body(
        &self,
        own: &[(Option<u32>, usize)],
        exit: Option<usize>,
    ) -> Option<Vec<String>> {
        let &(None, h) = own.last()? else { return None };
        let x = exit?;
        let stmts = &self.bir[h].0;
        let (
            Some(ir::Stmt::Assign(Expr::Var(d), Expr::Caught)),
            Some(ir::Stmt::Throw(Expr::Var(t))),
        ) = (stmts.first(), stmts.last())
        else {
            return None;
        };
        let v = self.ctx.vars.as_ref()?;
        if self.visited[h]
            || v.root[*d] != v.root[*t]
            || stmts.len() < 3
            || self.cfg.blocks[h].term != Term::Exit
        {
            return None;
        }
        let body: Vec<String> = stmts[1..stmts.len() - 1]
            .iter()
            .map(|s| self.ctx.stmt(s))
            .collect();
        let after = &self.bir[x].0;
        let have = self.skip.get(&x).copied().unwrap_or(0);
        let same = after.len() >= have + body.len()
            && after[have..have + body.len()]
                .iter()
                .map(|s| self.ctx.stmt(s))
                .eq(body.iter().cloned());
        same.then_some(body)
    }

    fn catch_clause(&mut self, ty: Option<u32>, h: usize, exit: Option<usize>, ind: usize) {
        {
            let t = ty
                .and_then(|t| self.ctx.dex.type_name(t).ok())
                .map_or("Throwable".into(), |t| type_ref(&t));
            let var = match self.bir[h].0.first() {
                Some(ir::Stmt::Assign(Expr::Var(d), Expr::Caught)) if !self.visited[h] => {
                    self.skip.insert(h, 1);
                    self.ctx.var_name(*d)
                }
                _ => "th".into(),
            };
            self.line(ind, format!("}} catch ({t} {var}) {{"));
            self.seq(h, exit, ind + 1);
        }
    }

    /// Emit blocks starting at `b` until `stop` is reached.
    fn seq(&mut self, mut b: usize, stop: Option<usize>, ind: usize) {
        loop {
            if Some(b) == stop {
                return;
            }
            let hs = &self.cfg.blocks[b].handlers;
            // nested try: inner handlers come first, then the enclosing ones
            let nested =
                |top: &Vec<(Option<u32>, usize)>| hs.len() > top.len() && hs.ends_with(top);
            if let Some(top) = self.trys.last()
                && hs != top
                && !nested(top)
            {
                self.try_exit.get_or_insert(b);
                return;
            }
            if !hs.is_empty()
                && self.trys.last() != Some(hs)
                && !self.visited[b]
                && self.jump_target(b).is_none()
            {
                match self.try_block(b, ind) {
                    Some(n) => {
                        b = n;
                        continue;
                    }
                    None => return,
                }
            }
            if let Some(j) = self.jump(b) {
                return self.line(ind, j);
            }
            if self.visited[b] && self.small_exit(b) {
                // a short `return`/`throw` tail is cheaper to repeat than to jump to
                let skip = self.skip.get(&b).copied().unwrap_or(0);
                for st in self.bir[b].0.iter().skip(skip) {
                    let l = self.ctx.stmt(st);
                    self.line(ind, l);
                }
                return;
            }
            // a short region that only continues where this construct ends: `if (A) { if (B) X else Y } else Y`
            if self.visited[b]
                && ind <= MAX_DEPTH
                && self.dups < 4
                && let Some(region) = self.small_region(b, stop)
            {
                let saved: Vec<bool> = region
                    .iter()
                    .map(|&r| std::mem::replace(&mut self.visited[r], false))
                    .collect();
                self.dups += 1;
                self.seq(b, stop, ind);
                self.dups -= 1;
                for (&r, v) in region.iter().zip(saved) {
                    self.visited[r] |= v;
                }
                return;
            }
            if self.visited[b] || ind > MAX_DEPTH {
                self.labels.insert(b);
                return self.line(ind, format!("goto {};", self.label(b)));
            }
            let next = if self.header[b] && !self.loops.iter().any(|l| l.0 == b) {
                let follow = self.follow[b];
                let at = self.lines.len();
                self.loops.push((b, follow));
                // header that only tests the loop condition -> `while (cond)`
                let simple_head = match self.cfg.blocks[b].term {
                    Term::If { taken, fall }
                        if self.bir[b].0.is_empty() && self.cfg.blocks[b].handlers.is_empty() =>
                    {
                        let c = self.bir[b].1.clone().unwrap_or(Expr::Raw("?".into()));
                        if Some(taken) == follow {
                            Some((c.negate(), fall))
                        } else if Some(fall) == follow {
                            Some((c, taken))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((c, body)) = simple_head {
                    self.visited[b] = true;
                    self.line(ind, format!("while ({}) {{", self.ctx.expr(&c, None)));
                    self.seq(body, None, ind + 1);
                } else {
                    self.line(ind, "while (true) {".into());
                    if let Some(n) = self.block(b, ind + 1, None) {
                        self.seq(n, None, ind + 1);
                    }
                }
                // the header's label belongs on the loop line
                self.starts.insert(b, at);
                self.loops.pop();
                if self.lines.last().is_some_and(|l| l.1 == "continue;") {
                    self.lines.pop();
                }
                self.line(ind, "}".into());
                follow
            } else {
                self.block(b, ind, stop)
            };
            match next {
                Some(n) => b = n,
                None => return,
            }
        }
    }

    /// `stop` = where the enclosing construct continues; branches that never rejoin stop there too.
    fn block(&mut self, b: usize, ind: usize, stop: Option<usize>) -> Option<usize> {
        self.visited[b] = true;
        self.starts.entry(b).or_insert(self.lines.len());
        let blk = &self.cfg.blocks[b];
        let (stmts, cond) = &self.bir[b];
        let skip = self.skip.get(&b).copied().unwrap_or(0).min(stmts.len());
        for s in &stmts[skip..] {
            let l = self.ctx.stmt(s);
            self.line(ind, l);
        }
        let join = self.cfg.ipdom[b].or(self.merge[b]);
        // both arms leave the method: they still end where the enclosing construct continues
        let arm_stop = join.or(stop);
        match &blk.term {
            Term::Exit => None,
            Term::Goto(t) => Some(*t),
            Term::If { taken, fall } => {
                let (taken, fall) = (*taken, *fall);
                let cond = cond.clone().unwrap_or(Expr::Raw("?".into()));
                if Some(fall) == arm_stop {
                    self.line(ind, format!("if ({}) {{", self.ctx.expr(&cond, None)));
                    self.seq(taken, arm_stop, ind + 1);
                } else {
                    let if_at = self.lines.len();
                    self.line(
                        ind,
                        format!("if ({}) {{", self.ctx.expr(&cond.clone().negate(), None)),
                    );
                    self.seq(fall, arm_stop, ind + 1);
                    // empty then-arm (it just left the enclosing try): flip to `if (cond) { else-arm }`
                    if self.lines.len() == if_at + 1 && Some(taken) != arm_stop {
                        self.lines[if_at].1 = format!("if ({}) {{", self.ctx.expr(&cond, None));
                        self.seq(taken, arm_stop, ind + 1);
                    } else if Some(taken) != arm_stop {
                        self.line(ind, "} else {".into());
                        self.seq(taken, arm_stop, ind + 1);
                    }
                }
                self.line(ind, "}".into());
                join
            }
            Term::Switch { cases, default } => {
                let (cases, default) = (cases.clone(), *default);
                let key = cond
                    .as_ref()
                    .map_or("?".into(), |c| self.ctx.expr(c, Some("I")));
                self.line(ind, format!("switch ({key}) {{"));
                let mut k = 0;
                while k < cases.len() {
                    let t = cases[k].1;
                    while k < cases.len() && cases[k].1 == t {
                        self.line(ind + 1, format!("case {}:", cases[k].0));
                        k += 1;
                    }
                    self.case_body(t, arm_stop, ind + 2);
                }
                if Some(default) != join {
                    self.line(ind + 1, "default:".into());
                    self.case_body(default, arm_stop, ind + 2);
                }
                self.line(ind, "}".into());
                join
            }
        }
    }

    /// Blocks from `b` whose every path reaches `stop` or leaves the method within a few small,
    /// loop-free blocks; such a region is cheaper to repeat than to jump to.
    fn small_region(&self, b: usize, stop: Option<usize>) -> Option<Vec<usize>> {
        let (mut region, mut todo, mut stmts) = (vec![], vec![b], 0);
        while let Some(x) = todo.pop() {
            if Some(x) == stop || region.contains(&x) {
                continue;
            }
            let blk = &self.cfg.blocks[x];
            stmts += self.bir[x].0.len();
            let same_try = blk.handlers.is_empty() || self.trys.last() == Some(&blk.handlers);
            if self.header[x]
                || !same_try
                || matches!(blk.term, Term::Switch { .. })
                || region.len() >= 4
                || stmts > 6
            {
                return None;
            }
            region.push(x);
            todo.extend(blk.succs());
        }
        Some(region)
    }

    fn small_exit(&self, b: usize) -> bool {
        let blk = &self.cfg.blocks[b];
        blk.term == Term::Exit
            && self.bir[b].0.len() <= 3
            && (blk.handlers.is_empty() || self.trys.last() == Some(&blk.handlers))
    }

    fn case_body(&mut self, t: usize, join: Option<usize>, ind: usize) {
        if Some(t) != join {
            self.seq(t, join, ind);
        }
        self.line(ind, "break;".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoists_declarations_used_after_their_block() {
        let mut l: Vec<(usize, String)> = vec![
            (2, "try {".into()),
            (3, "\u{1}a/Foo\u{2} foo = make();".into()),
            (3, "int i = 1;".into()),
            (2, "} finally {".into()),
            (3, "done();".into()),
            (2, "}".into()),
            (2, "use(foo);".into()),
        ];
        hoist_declarations(&mut l);
        assert_eq!(l[0], (2, "\u{1}a/Foo\u{2} foo;".into()));
        assert_eq!(l[2], (3, "foo = make();".into()));
        // i is not used after the block: left alone
        assert_eq!(l[3], (3, "int i = 1;".into()));
    }

    #[test]
    fn enum_rewrites() {
        assert_eq!(
            split_args("a, f(b, c), \"x, y\""),
            vec!["a", "f(b, c)", "\"x, y\""]
        );
        let ctor = "    private Color(String str, int i, int i2) {\n        super(str, i);\n        this.rgb = i2;\n    }\n";
        assert_eq!(
            enum_ctor(ctor),
            "    Color(int i2) {\n        this.rgb = i2;\n    }\n"
        );
        assert_eq!(
            replace_word("a[i] = c; cc = c2;", "c", "RED"),
            "a[i] = RED; cc = c2;"
        );
        assert_eq!(
            constant("RED", &["\"RED\"".into(), "0".into(), "255".into()]),
            Some((0, "RED(255)".into()))
        );
    }

    #[test]
    fn inner_names() {
        assert_eq!(inner_name("La/B$C;", "La/B;"), "C");
        assert_eq!(inner_name("La/B$1;", "La/B;"), "AnonymousClass1");
        assert_eq!(inner_name("La/B$$Lambda0;", "La/B;"), "$Lambda0");
    }

    #[test]
    fn loop_keeps_its_body() {
        let d = crate::dex::tests::tiny_dex();
        let dex = Dex::parse(&d).unwrap();
        // 0: if-eqz v0, 5 ; 2: add-int/lit8 v1, v1, 1 ; 4: goto 0 ; 5: return-void
        let units: Vec<u8> = [0x0038u16, 0x0005, 0x01d8, 0x0101, 0xfc28, 0x000e]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let insns = bytecode::decode(&units).unwrap();
        let mut cfg = cfg::build(&insns, &units, &[]).unwrap();
        let mut bir: Bir = cfg
            .blocks
            .iter()
            .map(|b| ir::lower(&dex, &insns, &units, b.first, b.last, &b.term).unwrap())
            .collect();
        opt::inline(&cfg, &mut bir, 2);
        if opt::merge_conditions(&mut cfg, &mut bir) {
            cfg.recompute();
        }
        let ctx = Ctx {
            dex: &dex,
            class: "La/b/Cls;",
            ret: "V".into(),
            vars: None,
        };
        let mut e = Emitter::new(&ctx, &insns, &cfg, &bir);
        e.seq(0, None, 2);
        let out = e.finish();
        assert!(
            out.contains(
                "while (v0 != 0) {
            v1 = v1 + 1;
        }"
            ),
            "{out}"
        );
    }

    #[test]
    fn loop_exits_meet_after_the_loop() {
        let d = crate::dex::tests::tiny_dex();
        let dex = Dex::parse(&d).unwrap();
        // 0: if-eqz v0, 7 ; 2: if-nez v1, 9 ; 4: add-int/lit8 v2, v2, 1 ; 6: goto 0
        // 7: const/4 v3, 0 ; 8: goto 10 ; 9: const/4 v3, 1 ; 10: return-void
        let units: Vec<u8> = [
            0x0038u16, 0x0007, 0x0139, 0x0007, 0x02d8, 0x0102, 0xfa28, 0x0312, 0x0228, 0x1312,
            0x000e,
        ]
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect();
        let insns = bytecode::decode(&units).unwrap();
        let cfg = cfg::build(&insns, &units, &[]).unwrap();
        let bir: Bir = cfg
            .blocks
            .iter()
            .map(|b| ir::lower(&dex, &insns, &units, b.first, b.last, &b.term).unwrap())
            .collect();
        let ctx = Ctx {
            dex: &dex,
            class: "La/b/Cls;",
            ret: "V".into(),
            vars: None,
        };
        let mut e = Emitter::new(&ctx, &insns, &cfg, &bir);
        e.seq(0, None, 2);
        let out = e.finish();
        assert!(
            !out.contains("goto") && out.contains("v3 = 0;") && out.contains("v3 = 1;"),
            "{out}"
        );
    }

    #[test]
    fn try_catch_block() {
        let d = crate::dex::tests::tiny_dex();
        let dex = Dex::parse(&d).unwrap();
        // 0: throw v1  [try 0..1, catch-all -> 1] ; 1: move-exception v0 ; 2: return-void
        let units: Vec<u8> = [0x0127u16, 0x000d, 0x000e]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let tries = [crate::dex::Try {
            start: 0,
            count: 1,
            handlers: vec![(None, 1)],
        }];
        let insns = bytecode::decode(&units).unwrap();
        let cfg = cfg::build(&insns, &units, &tries).unwrap();
        let bir: Bir = cfg
            .blocks
            .iter()
            .map(|b| ir::lower(&dex, &insns, &units, b.first, b.last, &b.term).unwrap())
            .collect();
        let ctx = Ctx {
            dex: &dex,
            class: "La/b/Cls;",
            ret: "V".into(),
            vars: None,
        };
        let mut e = Emitter::new(&ctx, &insns, &cfg, &bir);
        e.seq(0, None, 2);
        let out = e.finish();
        assert!(
            out.contains(
                "try {
            throw v1;
        } catch (Throwable th) {"
            ),
            "{out}"
        );
    }
}
