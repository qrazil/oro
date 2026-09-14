//! Embedded Oro-written stdlib modules.
//!
//! The README's growth path for the standard library is "written in Oro on
//! top of the frozen core", not baked into Rust. This is the mechanism that
//! makes that possible in a single self-contained binary: each module's
//! source is baked in at compile time with `include_str!` and resolved by
//! name here, then run through the *same* module-body machinery `import`
//! already uses for a user's own `.oro` files (`compile_source` + a module
//! frame with `ReturnAction::BuildModule`) — there is exactly one path that
//! turns Oro source into a cached module namespace, whether that source
//! shipped in the binary or lives next to the user's script.

/// Name -> embedded Oro source, for modules that ship inside the binary.
const MODULES: &[(&str, &str)] = &[
    ("io", include_str!("../../std/io.oro")),
    ("json", include_str!("../../std/json.oro")),
    ("http", include_str!("../../std/http.oro")),
    ("base64", include_str!("../../std/base64.oro")),
    ("html", include_str!("../../std/html.oro")),
];

/// Look up the embedded source for stdlib module `path`, or `None` if `path`
/// does not name one. A user file can never shadow one of these names —
/// callers must check this before falling back to the script-directory
/// search path.
pub fn source_for(path: &str) -> Option<&'static str> {
    MODULES.iter().find(|(name, _)| *name == path).map(|(_, src)| *src)
}

/// How a diagnostic names a frame from embedded module `path`.
///
/// `<std/http.oro>` rather than `std/http.oro`, for the reason the brackets
/// already mean everywhere else in this codebase (`<module>`, `<stdout>`,
/// `<stdin>`): what is inside them is a name, not a path you can open. The
/// module ships *inside the binary* — there is no `std/http.oro` next to the
/// user's script, and a bare path would be the same species of lie as the bug
/// this naming exists to fix: a location that looks openable and either misses
/// or, worse, hits an unrelated file that happens to sit at that relative path.
/// The text between the brackets is still the module's real home in the Oro
/// repository, so a reader who wants to see line 1117 knows exactly where it
/// is. CPython names its frozen modules the same way (`<frozen
/// importlib._bootstrap>`).
pub fn display_name(path: &str) -> String {
    format!("<std/{path}.oro>")
}
