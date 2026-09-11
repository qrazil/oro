//! Shared `re` operations over the linear-time `regex` engine. Both the
//! module-level functions (`re.search`, …) and the compiled-pattern methods
//! (`p.search`, …) go through here. Match spans are reported as character
//! offsets (Python semantics), converted from the crate's byte offsets.

use std::rc::Rc;

use crate::exc::{runtime_error, value_error, VErr};
use crate::value::{OroList, OroMatch, OroRegex, OroTuple, Value};

pub type RResult<T> = Result<T, VErr>;

/// Compile a pattern. Backreferences and lookaround are unsupported by the
/// linear-time engine and surface here as a clear error.
pub fn compile(pattern: &str) -> RResult<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| {
        value_error(format!(
            "invalid or unsupported regular expression '{pattern}': {e} \
             (note: backreferences and lookaround are not supported — they force \
             backtracking, which would break the linear-time guarantee)"
        ))
    })
}

pub fn regex_value(pattern: &str) -> RResult<Value> {
    let re = compile(pattern)?;
    Ok(Value::Regex(Rc::new(OroRegex { re, pattern: pattern.to_string() })))
}

/// Build a match value from a crate `Captures`, converting byte spans to char
/// offsets against the haystack `s`.
fn match_from_caps(caps: &regex::Captures, s: &str) -> Value {
    let mut groups = Vec::with_capacity(caps.len());
    for i in 0..caps.len() {
        match caps.get(i) {
            Some(m) => {
                let start = s[..m.start()].chars().count();
                let end = s[..m.end()].chars().count();
                groups.push(Some((start, end, m.as_str().to_string())));
            }
            None => groups.push(None),
        }
    }
    Value::Match(Rc::new(OroMatch { groups }))
}

pub fn search(re: &regex::Regex, s: &str) -> Value {
    match re.captures(s) {
        Some(caps) => match_from_caps(&caps, s),
        None => Value::None,
    }
}

/// Like `search`, but the whole string must match (`re.fullmatch`).
pub fn fullmatch(re: &regex::Regex, s: &str) -> Value {
    match re.captures(s) {
        Some(caps) => {
            let whole = caps.get(0).unwrap();
            if whole.start() == 0 && whole.end() == s.len() {
                return match_from_caps(&caps, s);
            }
            Value::None
        }
        None => Value::None,
    }
}

/// `re.findall`: whole matches with no groups; the single group with one group;
/// tuples of groups with several — matching CPython.
pub fn findall(re: &regex::Regex, s: &str) -> Value {
    let ngroups = re.captures_len() - 1;
    let mut out: Vec<Value> = Vec::new();
    for caps in re.captures_iter(s) {
        if ngroups == 0 {
            out.push(Value::str(caps.get(0).unwrap().as_str().to_string()));
        } else if ngroups == 1 {
            let g = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            out.push(Value::str(g.to_string()));
        } else {
            let tup: Vec<Value> = (1..=ngroups)
                .map(|i| Value::str(caps.get(i).map(|m| m.as_str()).unwrap_or("").to_string()))
                .collect();
            out.push(Value::Tuple(OroTuple::new(tup)));
        }
    }
    Value::List(OroList::new(out))
}

/// `re.finditer`: a list of match objects (iterable by `for`), each carrying
/// group spans and texts — the positions users need for two-pass techniques.
pub fn finditer(re: &regex::Regex, s: &str) -> Value {
    let out: Vec<Value> = re.captures_iter(s).map(|caps| match_from_caps(&caps, s)).collect();
    Value::List(OroList::new(out))
}

pub fn sub(re: &regex::Regex, repl: &str, s: &str) -> Value {
    let template = translate_repl(repl);
    Value::str(re.replace_all(s, template.as_str()).into_owned())
}

pub fn split(re: &regex::Regex, s: &str) -> Value {
    let out: Vec<Value> = re.split(s).map(|p| Value::str(p.to_string())).collect();
    Value::List(OroList::new(out))
}

/// `m.group(n)` — group 0 (or no arg) is the whole match; `None` if the group
/// did not participate.
pub fn group(m: &OroMatch, n: usize) -> RResult<Value> {
    match m.groups.get(n) {
        Some(Some((_, _, text))) => Ok(Value::str(text.clone())),
        Some(None) => Ok(Value::None),
        None => Err(runtime_error(format!("no such group: {n}"))),
    }
}

/// `m.start(n)` / `m.end(n)` — char offset, or -1 for a non-participating group.
pub fn start(m: &OroMatch, n: usize) -> RResult<Value> {
    span(m, n).map(|opt| Value::Int(opt.map(|(s, _)| s as i64).unwrap_or(-1)))
}

pub fn end(m: &OroMatch, n: usize) -> RResult<Value> {
    span(m, n).map(|opt| Value::Int(opt.map(|(_, e)| e as i64).unwrap_or(-1)))
}

fn span(m: &OroMatch, n: usize) -> RResult<Option<(usize, usize)>> {
    match m.groups.get(n) {
        Some(Some((s, e, _))) => Ok(Some((*s, *e))),
        Some(None) => Ok(None),
        None => Err(runtime_error(format!("no such group: {n}"))),
    }
}

/// Translate a Python-style replacement (`\1`, `\g<name>`, `\\`, `\n`) into the
/// `regex` crate's `${...}` template form, escaping literal `$`.
fn translate_repl(repl: &str) -> String {
    let chars: Vec<char> = repl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' {
            out.push_str("$$"); // literal $ in Python repl
            i += 1;
        } else if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            if n.is_ascii_digit() {
                let mut j = i + 1;
                let mut num = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    num.push(chars[j]);
                    j += 1;
                }
                out.push_str(&format!("${{{num}}}"));
                i = j;
            } else if n == 'g' && chars.get(i + 2) == Some(&'<') {
                let mut j = i + 3;
                let mut name = String::new();
                while j < chars.len() && chars[j] != '>' {
                    name.push(chars[j]);
                    j += 1;
                }
                out.push_str(&format!("${{{name}}}"));
                i = j + 1; // consume '>'
            } else {
                let decoded = match n {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    other => other,
                };
                out.push(decoded);
                i += 2;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}
