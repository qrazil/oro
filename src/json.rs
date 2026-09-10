//! JSON, in Rust: the per-byte half of `std/json.oro`.
//!
//! Why this is Rust and `std/http.oro` is not — the argument in full is
//! `docs/stdlib-server-design.md` §5 — is that JSON parsing *is* the per-byte
//! loop §5's own rule sends to Rust, while HTTP head parsing decomposes into
//! three generic `bytes` calls and per-line Oro work. Nothing here is policy: a
//! JSON document has exactly one reading, so there is no decision for a program
//! to want to override, which is the other half of why this can be frozen and
//! an `http` builtin cannot.
//!
//! Two disciplines the VM keeps and this file keeps with it:
//!
//! * **No Rust recursion.** Both directions walk an explicit stack, because a
//!   document arriving off a socket chooses the nesting depth and the VM never
//!   lets Oro-level nesting reach the Rust stack.
//! * **Errors are the module's own.** [`classify`] owns the mapping from these
//!   messages to exception classes, the way `crate::net` owns its errno table,
//!   so no format-specific string lands in the VM's general table.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::value::{OroDict, Value};

/// The deepest nesting `parse` accepts, and the deepest `stringify` will walk.
///
/// Not a stack bound — neither direction recurses in Rust — but a bound on
/// what a *client* can make a server allocate. `[` costs one input byte and
/// buys a heap container, so unbounded nesting turns a 200 KB request into
/// 200 000 live `Rc` allocations; capping the depth caps that amplification
/// regardless of how the input is shaped.
///
/// 10 000 is three orders of magnitude above anything a real document reaches,
/// and the same order as CPython's own ceiling (its C accelerator takes 5 000
/// and raises `RecursionError` at 20 000), so a document CPython reads Oro
/// reads. See §5 for what it changes.
pub const MAX_DEPTH: usize = 10_000;

/// Map a message this module produced to its exception class.
///
/// The VM's `classify_error` is a table of substrings owned by the messages
/// that exist; a codec's messages should not be in it, so — exactly as
/// `crate::net::classify` does for socket errnos — this owns them here.
pub fn classify(msg: &str) -> Option<&'static str> {
    // Every parse diagnostic ends in the character offset, and every one of
    // them is a `ValueError`: `json.parse` is documented to raise one naming
    // the offset, and a server that wraps a body parse in `except ValueError`
    // to answer 400 must keep catching all of them.
    if msg.ends_with(char::is_numeric) && msg.contains(" at position ") {
        return Some("ValueError");
    }
    if msg.ends_with("is not JSON serializable") {
        // `nan`/`Infinity` are a ValueError (the value cannot be written);
        // an unsupported *type* is a TypeError. `_stringify_value` has always
        // drawn it there and CPython draws it there too.
        return Some(if msg.starts_with("object of type ") { "TypeError" } else { "ValueError" });
    }
    None
}

// --- decoding ----------------------------------------------------------------

/// A container being built. The parser's own stack, so nesting costs heap and
/// never the Rust stack.
enum Partial {
    Array(Vec<Value>),
    /// The dict so far, and the key whose value is being read.
    Object(OroDict, Value),
}

struct Parser<'a> {
    text: &'a str,
    b: &'a [u8],
    i: usize,
}

/// `parse(text)`: a JSON document to an Oro value.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser { text, b: text.as_bytes(), i: 0 };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.i < p.b.len() {
        return Err(p.unexpected_here());
    }
    Ok(value)
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    /// The character offset of byte `at`. Diagnostics count characters, because
    /// the parser they replace indexed the text by character; this is the only
    /// place the two ever have to agree, and it runs once, on the way to
    /// raising.
    fn char_pos(&self, at: usize) -> usize {
        self.text[..at].chars().count()
    }

    fn fail_at<T>(&self, msg: impl std::fmt::Display, at: usize) -> Result<T, String> {
        Err(format!("{msg} at position {}", self.char_pos(at)))
    }

    fn fail<T>(&self, msg: impl std::fmt::Display) -> Result<T, String> {
        self.fail_at(msg, self.i)
    }

    /// The character at `self.i` — for the diagnostics that quote it. `self.i`
    /// is always on a character boundary where one is quoted.
    fn char_here(&self) -> char {
        self.text[self.i..].chars().next().unwrap_or('\u{0}')
    }

    fn unexpected_here(&self) -> String {
        format!("unexpected '{}' at position {}", self.char_here(), self.char_pos(self.i))
    }

    /// Read one complete value, iteratively. The only entry point: nested
    /// containers are pushed onto `stack`, never onto the Rust stack.
    fn value(&mut self) -> Result<Value, String> {
        let mut stack: Vec<Partial> = Vec::new();
        // Whether the next thing to read is an object key rather than a value.
        let mut want_key = false;

        'read: loop {
            if want_key {
                want_key = false;
                self.skip_ws();
                if self.i >= self.b.len() || self.b[self.i] != b'"' {
                    return self.fail("expected a string key");
                }
                let key = Value::str(self.string()?);
                self.skip_ws();
                if self.i >= self.b.len() || self.b[self.i] != b':' {
                    return self.fail("expected ':'");
                }
                self.i += 1;
                match stack.last_mut() {
                    Some(Partial::Object(_, k)) => *k = key,
                    _ => unreachable!("a key is only ever read inside an object"),
                }
            }

            self.skip_ws();
            if self.i >= self.b.len() {
                return self.fail("unexpected end of input");
            }
            let mut value = match self.b[self.i] {
                b'{' => {
                    self.i += 1;
                    self.skip_ws();
                    if self.i < self.b.len() && self.b[self.i] == b'}' {
                        self.i += 1;
                        Value::Dict(Rc::new(RefCell::new(OroDict::new())))
                    } else {
                        self.push(&mut stack, Partial::Object(OroDict::new(), Value::None))?;
                        want_key = true;
                        continue 'read;
                    }
                }
                b'[' => {
                    self.i += 1;
                    self.skip_ws();
                    if self.i < self.b.len() && self.b[self.i] == b']' {
                        self.i += 1;
                        Value::List(Rc::new(RefCell::new(Vec::new())))
                    } else {
                        self.push(&mut stack, Partial::Array(Vec::new()))?;
                        continue 'read;
                    }
                }
                b'"' => Value::str(self.string()?),
                b'-' | b'0'..=b'9' => self.number()?,
                // The three JSON words are spelled exactly as Oro spells its
                // literals, so this is a scan and not a translation.
                _ if self.b[self.i..].starts_with(b"true") => {
                    self.i += 4;
                    Value::Bool(true)
                }
                _ if self.b[self.i..].starts_with(b"false") => {
                    self.i += 5;
                    Value::Bool(false)
                }
                _ if self.b[self.i..].starts_with(b"null") => {
                    self.i += 4;
                    Value::None
                }
                _ => return Err(self.unexpected_here()),
            };

            // A value is finished: hand it to the container underneath, and
            // keep handing containers up for as long as they close here.
            loop {
                match stack.last_mut() {
                    None => return Ok(value),
                    Some(Partial::Array(items)) => {
                        items.push(value);
                        self.skip_ws();
                        if self.i >= self.b.len() {
                            return self.fail("unterminated array");
                        }
                        match self.b[self.i] {
                            b',' => {
                                self.i += 1;
                                continue 'read;
                            }
                            b']' => {
                                self.i += 1;
                                let Some(Partial::Array(items)) = stack.pop() else {
                                    unreachable!("just matched an array")
                                };
                                value = Value::List(Rc::new(RefCell::new(items)));
                            }
                            _ => return Err(self.unexpected_here()),
                        }
                    }
                    Some(Partial::Object(dict, key)) => {
                        // A repeated key overwrites, keeping the position of
                        // the first — `OroDict::insert`'s rule, and the one
                        // `result[key] = ...` already had.
                        dict.insert(std::mem::replace(key, Value::None), value)?;
                        self.skip_ws();
                        if self.i >= self.b.len() {
                            return self.fail("unterminated object");
                        }
                        match self.b[self.i] {
                            b',' => {
                                self.i += 1;
                                want_key = true;
                                continue 'read;
                            }
                            b'}' => {
                                self.i += 1;
                                let Some(Partial::Object(dict, _)) = stack.pop() else {
                                    unreachable!("just matched an object")
                                };
                                value = Value::Dict(Rc::new(RefCell::new(dict)));
                            }
                            _ => return Err(self.unexpected_here()),
                        }
                    }
                }
            }
        }
    }

    fn push(&self, stack: &mut Vec<Partial>, p: Partial) -> Result<(), String> {
        if stack.len() >= MAX_DEPTH {
            // The opening bracket is one byte back, and it is the one to name.
            return self.fail_at(
                format!("maximum nesting depth ({MAX_DEPTH}) exceeded"),
                self.i - 1,
            );
        }
        stack.push(p);
        Ok(())
    }

    /// A string literal, with `self.i` on its opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            // Copy the longest run that needs no decoding in one go — the whole
            // string, for the overwhelming majority of keys and values.
            let start = self.i;
            while self.i < self.b.len() {
                let c = self.b[self.i];
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > start {
                out.push_str(&self.text[start..self.i]);
            }
            if self.i >= self.b.len() {
                return self.fail("unterminated string");
            }
            match self.b[self.i] {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => self.escape(&mut out)?,
                _ => return self.fail("invalid control character in string"),
            }
        }
    }

    /// One escape sequence, with `self.i` on its backslash.
    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        self.i += 1;
        if self.i >= self.b.len() {
            return self.fail("unterminated escape");
        }
        let simple = match self.b[self.i] {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(out),
            _ => return self.fail(format!("invalid escape '\\{}'", self.char_here())),
        };
        self.i += 1;
        out.push(simple);
        Ok(())
    }

    /// The four hex digits of a `\uXXXX`, with `self.i` on the `u`.
    fn hex4(&mut self) -> Result<u32, String> {
        self.i += 1;
        // The count is in *characters*: the parser this replaces sliced four of
        // them, and its "incomplete" test was `pos + 4 > len` over the same
        // units. Four non-ASCII characters are still four characters, and the
        // diagnostic quotes them.
        let mut end = self.i;
        for _ in 0..4 {
            match self.text[end..].chars().next() {
                Some(c) => end += c.len_utf8(),
                None => return self.fail("incomplete \\u escape"),
            }
        }
        let digits = &self.text[self.i..end];
        let mut cp: u32 = 0;
        for c in digits.chars() {
            match c.to_digit(16) {
                Some(d) => cp = cp * 16 + d,
                None => return self.fail(format!("invalid \\u escape '{digits}'")),
            }
        }
        self.i = end;
        Ok(cp)
    }

    fn unicode_escape(&mut self, out: &mut String) -> Result<(), String> {
        const HIGH: std::ops::RangeInclusive<u32> = 0xD800..=0xDBFF;
        const LOW: std::ops::RangeInclusive<u32> = 0xDC00..=0xDFFF;

        let cp = self.hex4()?;
        if HIGH.contains(&cp) {
            if self.i + 1 < self.b.len()
                && self.b[self.i] == b'\\'
                && self.b[self.i + 1] == b'u'
            {
                let save = self.i;
                self.i += 1;
                let low = self.hex4()?;
                if LOW.contains(&low) {
                    let combined = 0x10000 + (cp - 0xD800) * 0x400 + (low - 0xDC00);
                    out.push(char::from_u32(combined).expect("a paired surrogate is a scalar"));
                    return Ok(());
                }
                self.i = save;
            }
            return self.fail("unpaired UTF-16 surrogate in \\u escape");
        }
        if LOW.contains(&cp) {
            return self.fail("unpaired UTF-16 surrogate in \\u escape");
        }
        out.push(char::from_u32(cp).expect("a non-surrogate below 0x10000 is a scalar"));
        Ok(())
    }

    /// A number literal. Anything holding `.`, `e` or `E` is a float; everything
    /// else is an int, promoted past `i64` the way every other Oro integer is.
    fn number(&mut self) -> Result<Value, String> {
        let digit = |c: u8| c.is_ascii_digit();
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        if self.i >= self.b.len() || !digit(self.b[self.i]) {
            return self.fail("invalid number");
        }
        if self.b[self.i] == b'0' {
            self.i += 1;
            if self.i < self.b.len() && digit(self.b[self.i]) {
                return self
                    .fail("invalid number: a leading zero cannot be followed by more digits");
            }
        } else {
            while self.i < self.b.len() && digit(self.b[self.i]) {
                self.i += 1;
            }
        }
        let mut is_float = false;
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            is_float = true;
            self.i += 1;
            if self.i >= self.b.len() || !digit(self.b[self.i]) {
                return self.fail("invalid number: expected a digit after '.'");
            }
            while self.i < self.b.len() && digit(self.b[self.i]) {
                self.i += 1;
            }
        }
        if self.i < self.b.len() && matches!(self.b[self.i], b'e' | b'E') {
            is_float = true;
            self.i += 1;
            if self.i < self.b.len() && matches!(self.b[self.i], b'+' | b'-') {
                self.i += 1;
            }
            if self.i >= self.b.len() || !digit(self.b[self.i]) {
                return self.fail("invalid number: expected a digit in the exponent");
            }
            while self.i < self.b.len() && digit(self.b[self.i]) {
                self.i += 1;
            }
        }
        let literal = &self.text[start..self.i];
        if is_float {
            // `to_float()`'s parse, including its answer for an exponent that
            // overflows: `1e400` is `inf`, as it is in CPython's `json`.
            return Ok(Value::Float(literal.parse::<f64>().unwrap_or(f64::NAN)));
        }
        // `to_int()`'s parse: `i64` if it fits, bignum if it does not.
        if let Ok(n) = literal.parse::<i64>() {
            return Ok(Value::Int(n));
        }
        let (neg, digits) = match literal.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, literal),
        };
        match BigInt::parse_decimal(digits) {
            Some(b) => Ok(Value::from_bigint(if neg { b.neg() } else { b })),
            None => self.fail("invalid number"),
        }
    }
}

// --- encoding ----------------------------------------------------------------

/// One step of the encoder's own stack. Same discipline as the parser: a value
/// nested a thousand deep costs a thousand of these and no Rust frames.
enum Job {
    /// Write this value at this indent level.
    Value(Value, usize),
    /// Write element `at` of a list or tuple (`at == len` closes it).
    Elem { seq: Value, at: usize, level: usize },
    /// Write entry `at` of a dict (`at == len` closes it).
    Entry { dict: Rc<RefCell<OroDict>>, at: usize, level: usize },
}

/// `stringify(value, indent)`: an Oro value to JSON text. `indent` is the
/// module's optional argument, already unwrapped from `null`.
pub fn stringify(value: &Value, indent: Option<&Value>) -> Result<String, String> {
    let mut out = String::new();
    let mut jobs = vec![Job::Value(value.clone(), 0)];
    while let Some(job) = jobs.pop() {
        if jobs.len() > MAX_DEPTH {
            // A cycle, or a structure deeper than anything JSON should hold.
            // The message is the one the Oro encoder's own runaway recursion
            // produced, so `except RuntimeError` still catches it.
            return Err("maximum recursion depth exceeded".to_string());
        }
        match job {
            Job::Value(v, level) => write_value(&mut out, &mut jobs, v, level, indent)?,
            Job::Elem { seq, at, level } => {
                let len = seq_len(&seq);
                if at == len {
                    close(&mut out, ']', level, indent)?;
                } else {
                    out.push_str(if at == 0 { "" } else { "," });
                    if indent.is_some() && at > 0 {
                        newline_pad(&mut out, level + 1, indent)?;
                    }
                    let item = seq_get(&seq, at);
                    jobs.push(Job::Elem { seq, at: at + 1, level });
                    jobs.push(Job::Value(item, level + 1));
                }
            }
            Job::Entry { dict, at, level } => {
                let len = dict.borrow().len();
                if at == len {
                    close(&mut out, '}', level, indent)?;
                } else {
                    out.push_str(if at == 0 { "" } else { "," });
                    if indent.is_some() && at > 0 {
                        newline_pad(&mut out, level + 1, indent)?;
                    }
                    let (key, item) = dict.borrow().items()[at].clone();
                    // Keys must already be strings. Guessing a conversion for
                    // an int or a bool key — which `json.dumps` does — is
                    // exactly the second spelling the language avoids.
                    let Value::Str(k) = &key else {
                        return Err(format!("keys must be str, not {}", class_label(&key)));
                    };
                    escape_into(&mut out, &k.s);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    jobs.push(Job::Entry { dict, at: at + 1, level });
                    jobs.push(Job::Value(item, level + 1));
                }
            }
        }
    }
    Ok(out)
}

fn write_value(
    out: &mut String,
    jobs: &mut Vec<Job>,
    v: Value,
    level: usize,
    indent: Option<&Value>,
) -> Result<(), String> {
    match &v {
        // `null`, `true` and `false` are Oro's literals *and* JSON's words, so
        // the value's own str() is already the wire form.
        Value::None | Value::Bool(_) | Value::Int(_) | Value::Big(_) => out.push_str(&v.repr()),
        Value::Float(f) => {
            if f.is_nan() {
                return Err("nan is not JSON serializable".to_string());
            }
            if f.is_infinite() {
                return Err("Infinity is not JSON serializable".to_string());
            }
            out.push_str(&v.repr());
        }
        Value::Str(s) => escape_into(out, &s.s),
        Value::List(_) | Value::Tuple(_) => {
            if seq_len(&v) == 0 {
                out.push_str("[]");
            } else {
                out.push('[');
                if indent.is_some() {
                    newline_pad(out, level + 1, indent)?;
                }
                jobs.push(Job::Elem { seq: v, at: 0, level });
            }
        }
        Value::Dict(d) => {
            if d.borrow().is_empty() {
                out.push_str("{}");
            } else {
                out.push('{');
                if indent.is_some() {
                    newline_pad(out, level + 1, indent)?;
                }
                jobs.push(Job::Entry { dict: d.clone(), at: 0, level });
            }
        }
        other => {
            return Err(format!("object of type {} is not JSON serializable", class_label(other)))
        }
    }
    Ok(())
}

fn close(out: &mut String, bracket: char, level: usize, indent: Option<&Value>) -> Result<(), String> {
    if indent.is_some() {
        newline_pad(out, level, indent)?;
    }
    out.push(bracket);
    Ok(())
}

/// A newline and `indent * level` spaces.
///
/// `indent` is only ever *used* here, which is why a non-int indent is an error
/// only when there is a non-empty container to indent — `stringify(1, indent=…)`
/// and `stringify([], indent=…)` never look at it. That is where the Oro
/// encoder's `" " * (indent * (level + 1))` put it, so the error it raises for a
/// str or a float indent is the same one, from the same multiply.
fn newline_pad(out: &mut String, level: usize, indent: Option<&Value>) -> Result<(), String> {
    let n = match indent {
        Some(Value::Int(n)) => *n,
        Some(Value::Bool(b)) => *b as i64,
        Some(other) => {
            return Err(format!(
                "unsupported operand type(s) for *: 'str' and '{}'",
                other.type_name()
            ))
        }
        None => 0,
    };
    out.push('\n');
    let width = n.saturating_mul(level as i64).max(0) as usize;
    out.extend(std::iter::repeat_n(' ', width));
    Ok(())
}

fn seq_len(v: &Value) -> usize {
    match v {
        Value::List(l) => l.borrow().len(),
        Value::Tuple(t) => t.len(),
        _ => 0,
    }
}

fn seq_get(v: &Value, i: usize) -> Value {
    match v {
        Value::List(l) => l.borrow()[i].clone(),
        Value::Tuple(t) => t[i].clone(),
        _ => Value::None,
    }
}

/// What `type(v)` prints, which is what both encoder diagnostics name.
fn class_label(v: &Value) -> String {
    match v {
        Value::Instance(i) => format!("<class '{}'>", i.class.name),
        other => format!("<class '{}'>", other.type_name()),
    }
}

/// A JSON string literal, quotes included.
///
/// Only what RFC 8259 requires: the two mandatory escapes, the five short forms
/// for the control characters that have one, and `\u00xx` for the rest of C0.
/// Everything else — including every non-ASCII character, and DEL — is written
/// through, because the output is UTF-8 text and `ensure_ascii` is a second
/// spelling of the same document.
fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    let b = s.as_bytes();
    let mut run = 0;
    for i in 0..b.len() {
        let c = b[i];
        if c >= 0x20 && c != b'"' && c != b'\\' {
            continue;
        }
        out.push_str(&s[run..i]);
        run = i + 1;
        match c {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            other => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0xf) as usize] as char);
            }
        }
    }
    out.push_str(&s[run..]);
    out.push('"');
}

#[cfg(test)]
mod tests;
