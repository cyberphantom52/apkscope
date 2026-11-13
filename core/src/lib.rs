//! The editing core of apkscope, for tools that patch APKs themselves: read a DEX, turn a class
//! into Smali and back, edit resources, rebuild the APK and sign it. The decompiler, the search
//! engine, the command line and the viewer live in the `apkscope` crate, on top of this one.
//!
//! Every fallible function returns [`Error`], whose text is what the command prints.

pub mod apk;
pub mod apksign;
pub mod arsc;
pub mod axml;
pub mod bytecode;
pub mod dex;
mod error;
pub mod rebuild;
pub mod res;
pub mod sign;
pub mod smali;
pub mod table;

pub use error::{Error, Result};
