//! The Oro programming language.
//!
//! Oro is a small, Python-inspired scripting language implemented in Rust and
//! executed on a bytecode virtual machine. This crate is the language library;
//! the `oro` binary (see `src/main.rs`) is a thin front-end over it.
//!
//! The lexer and parser are implemented so far; the compiler and VM are
//! intentionally absent.

//! ## `unsafe`, and where the one block of it lives
//!
//! This crate had no `unsafe` in it at all, and that was worth keeping — so it
//! is now **enforced rather than asserted**. `deny(unsafe_code)` applies to
//! every module here; exactly one, [`net::reuseport`], carries an
//! `#[allow(unsafe_code)]`, and the compiler is what stops a second one
//! appearing. The reasoning for spending it — four syscalls that std gives no
//! way to reach — is in that module's own docs and in `Cargo.toml`.
#![deny(unsafe_code)]

pub mod ast;
pub mod bigint;
pub mod builtins;
pub mod compiler;
pub mod exc;
pub mod fmt;
pub mod format;
pub mod json;
pub mod lexer;
pub mod linter;
pub mod net;
pub mod regexutil;
pub mod parser;
pub mod stream;
pub mod task;
pub mod value;
pub mod vm;
