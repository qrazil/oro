//! The Oro programming language.
//!
//! Oro is a small, Python-inspired scripting language implemented in Rust and
//! executed on a bytecode virtual machine. This crate is the language library;
//! the `oro` binary (see `src/main.rs`) is a thin front-end over it.
//!
//! The lexer and parser are implemented so far; the compiler and VM are
//! intentionally absent.

pub mod ast;
pub mod bigint;
pub mod builtins;
pub mod compiler;
pub mod fmt;
pub mod format;
pub mod lexer;
pub mod net;
pub mod regexutil;
pub mod parser;
pub mod stream;
pub mod value;
pub mod vm;
