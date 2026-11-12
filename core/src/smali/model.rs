//! Symbolic class model: every reference is by name, not by DEX index, so a class can move
//! between DEX files or be edited as Smali and written back with fresh, sorted tables.

use std::sync::Arc;

/// MUTF-8 bytes as stored in the DEX (keeps lone surrogates exactly).
pub type S = Arc<[u8]>;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Proto {
    pub ret: S,
    pub params: Vec<S>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FieldRef {
    pub class: S,
    pub name: S,
    pub ty: S,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MethodRef {
    pub class: S,
    pub name: S,
    pub proto: Proto,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Member {
    Field(FieldRef),
    Method(MethodRef),
}

/// `method_handle_item`: kinds 0-3 are field accessors, 4-8 invokes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Handle {
    pub kind: u16,
    pub member: Member,
}

/// `encoded_value`; floats are kept as bits so values can be hashed and compared exactly.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Value {
    Byte(i8),
    Short(i16),
    Char(u16),
    Int(i32),
    Long(i64),
    Float(u32),
    Double(u64),
    MethodType(Proto),
    MethodHandle(Box<Handle>),
    Str(S),
    Type(S),
    Field(FieldRef),
    Method(MethodRef),
    Enum(FieldRef),
    Array(Vec<Value>),
    Anno(Anno),
    Null,
    Bool(bool),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Anno {
    /// 0 build, 1 runtime, 2 system (unused inside a sub-annotation)
    pub vis: u8,
    pub ty: S,
    pub elems: Vec<(S, Value)>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ref {
    None,
    Str(S),
    Type(S),
    Field(FieldRef),
    Method(MethodRef),
    Proto(Proto),
    /// bootstrap handle, name, method type, extra arguments
    CallSite(Arc<Vec<Value>>),
    Handle(Handle),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Ins {
    pub op: u8,
    /// for /range forms regs[0] is the first register and n the count
    pub regs: [u16; 5],
    pub n: u8,
    pub lit: i64,
    /// label id of a branch or payload
    pub target: u32,
    pub r: Ref,
    /// proto of invoke-polymorphic
    pub proto: Option<Proto>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Item {
    Label(u32),
    Ins(Ins),
    /// targets are label ids, relative to the switch that uses this payload
    Packed {
        first: i32,
        targets: Vec<u32>,
    },
    Sparse(Vec<(i32, u32)>),
    Array {
        width: u16,
        data: Vec<u8>,
    },
    Line(u32),
    Local {
        reg: u16,
        name: Option<S>,
        ty: Option<S>,
        sig: Option<S>,
    },
    EndLocal(u16),
    Restart(u16),
    Prologue,
    Epilogue,
    SetFile(Option<S>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Try {
    pub start: u32,
    pub end: u32,
    /// (exception type or None for catch-all, handler label)
    pub handlers: Vec<(Option<S>, u32)>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Code {
    pub registers: u16,
    pub items: Vec<Item>,
    pub tries: Vec<Try>,
    /// debug_info present (even with no events)
    pub debug: bool,
    /// debug parameter names, one per parameter
    pub param_names: Vec<Option<S>>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Field {
    pub name: S,
    pub ty: S,
    pub access: u32,
    /// hiddenapi flags, when the DEX carries them for this class
    pub hidden: Option<u32>,
    /// static initial value from static_values
    pub init: Option<Value>,
    pub annos: Vec<Anno>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Method {
    pub name: S,
    pub proto: Proto,
    pub access: u32,
    /// hiddenapi flags, when the DEX carries them for this class
    pub hidden: Option<u32>,
    pub code: Option<Code>,
    pub annos: Vec<Anno>,
    /// one list per parameter; None when no parameter has any
    pub param_annos: Option<Vec<Vec<Anno>>>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Class {
    pub name: S,
    pub access: u32,
    pub sup: Option<S>,
    pub ifaces: Vec<S>,
    pub source: Option<S>,
    pub annos: Vec<Anno>,
    pub fields: Vec<Field>,
    pub methods: Vec<Method>,
}

pub const ACC_STATIC: u32 = 0x8;
pub const ACC_PRIVATE: u32 = 0x2;
pub const ACC_CONSTRUCTOR: u32 = 0x10000;

impl Method {
    /// direct methods: static, private or constructors
    pub fn is_direct(&self) -> bool {
        self.access & (ACC_STATIC | ACC_PRIVATE | ACC_CONSTRUCTOR) != 0
    }
    /// registers taken by the arguments, `this` included
    pub fn ins(&self) -> u16 {
        let words: u16 = self.proto.params.iter().map(|p| width(p)).sum();
        words + u16::from(self.access & ACC_STATIC == 0)
    }
}

/// registers a value of type `t` takes
pub fn width(t: &[u8]) -> u16 {
    if matches!(t, b"J" | b"D") { 2 } else { 1 }
}

pub fn s(b: &[u8]) -> S {
    Arc::from(b)
}

/// MUTF-8 bytes to UTF-16 units.
pub fn utf16(b: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as u16;
        let nx = |k: usize| b.get(i + k).map_or(0, |&x| x as u16 & 0x3f);
        let (u, n) = match c {
            0..=0x7f => (c, 1),
            0xc0..=0xdf => ((c & 0x1f) << 6 | nx(1), 2),
            0xe0..=0xef => ((c & 0x0f) << 12 | nx(1) << 6 | nx(2), 3),
            _ => (0xfffd, 1),
        };
        out.push(u);
        i += n;
    }
    out
}

/// UTF-16 units to MUTF-8 bytes.
pub fn mutf8(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    for &u in units {
        match u {
            1..=0x7f => out.push(u as u8),
            0 | 0x80..=0x7ff => out.extend([0xc0 | (u >> 6) as u8, 0x80 | (u & 0x3f) as u8]),
            _ => out.extend([
                0xe0 | (u >> 12) as u8,
                0x80 | ((u >> 6) & 0x3f) as u8,
                0x80 | (u & 0x3f) as u8,
            ]),
        }
    }
    out
}
