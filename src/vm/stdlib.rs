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
const MODULES: &[(&str, &str)] = &[("json", include_str!("../../std/json.oro"))];

/// Look up the embedded source for stdlib module `path`, or `None` if `path`
/// does not name one. A user file can never shadow one of these names —
/// callers must check this before falling back to the script-directory
/// search path.
pub fn source_for(path: &str) -> Option<&'static str> {
    MODULES.iter().find(|(name, _)| *name == path).map(|(_, src)| *src)
}
