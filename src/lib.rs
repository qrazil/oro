//! The Oro programming language.
//!
//! Oro is a small, Python-inspired scripting language implemented in Rust and
//! executed on a bytecode virtual machine. This crate is the language library;
//! the `oro` binary (see `src/main.rs`) is a thin front-end over it.
//!
//! Only the lexer is implemented so far. Everything downstream (parser,
//! compiler, VM) is intentionally absent.

pub mod lexer;
