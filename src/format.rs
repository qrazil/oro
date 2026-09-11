//! Python format-spec mini-language for f-strings and `format()`.
//!
//! [`format_value`] takes a value, an optional `!r`/`!s` conversion, and a
//! format-spec string, and returns the formatted text — matching CPython 3.12
//! for the spec grammar
//! `[[fill]align][sign][#][0][width][grouping][.precision][type]` over the
//! types `s d f e g x o b %` (plus the `E`/`F`/`G`/`X` uppercase variants).
//!
//! The formatter never calls back into Oro, so the flat VM loop is undisturbed.

use crate::exc::{runtime_error, value_error};
use crate::value::{VResult, Value};

/// Conversion requested by `!r`/`!s`/`!a` (0 = none). Kept as a `u8` so it can
/// ride inside the [`crate::compiler::Op::FormatValue`] opcode.
pub const CONV_NONE: u8 = 0;
pub const CONV_STR: u8 = 1;
pub const CONV_REPR: u8 = 2;
pub const CONV_ASCII: u8 = 3;

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,   // '<'
    Right,  // '>'
    Center, // '^'
    Sign,   // '=' — pad between the sign and the digits
}

#[derive(Clone, Copy, PartialEq)]
enum Sign {
    Minus, // '-' (default): sign only on negatives
    Plus,  // '+'
    Space, // ' '
}

struct Spec {
    fill: char,
    align: Option<Align>,
    sign: Sign,
    alt: bool,
    zero: bool,
    width: Option<usize>,
    grouping: Option<char>,
    precision: Option<usize>,
    ty: Option<char>,
}

/// Apply an f-string conversion then a format spec to `value`.
pub fn format_value(value: &Value, conv: u8, spec: &str) -> VResult<String> {
    // A conversion replaces the value with its string form first; the spec then
    // formats that string (so `{x!r:>10}` right-pads the repr).
    let converted: Value;
    let target = match conv {
        CONV_NONE => value,
        CONV_STR => {
            converted = Value::str(value.display());
            &converted
        }
        CONV_REPR => {
            converted = Value::str(value.repr());
            &converted
        }
        CONV_ASCII => {
            converted = Value::str(ascii_repr(&value.repr()));
            &converted
        }
        _ => value,
    };

    // Fast path: no spec is just `str()` of the (possibly converted) value.
    if spec.is_empty() {
        return Ok(target.display());
    }

    let spec = parse_spec(spec)?;
    apply(target, &spec)
}

// --- Spec parsing -----------------------------------------------------------

fn parse_spec(s: &str) -> VResult<Spec> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut spec = Spec {
        fill: ' ',
        align: None,
        sign: Sign::Minus,
        alt: false,
        zero: false,
        width: None,
        grouping: None,
        precision: None,
        ty: None,
    };

    let is_align = |c: char| matches!(c, '<' | '>' | '^' | '=');
    let to_align = |c: char| match c {
        '<' => Align::Left,
        '>' => Align::Right,
        '^' => Align::Center,
        '=' => Align::Sign,
        _ => unreachable!(),
    };

    // [[fill]align]: a fill char is only recognised when an align char follows.
    if chars.len() >= 2 && is_align(chars[1]) {
        spec.fill = chars[0];
        spec.align = Some(to_align(chars[1]));
        i = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        spec.align = Some(to_align(chars[0]));
        i = 1;
    }

    // [sign]
    if let Some(&c) = chars.get(i) {
        match c {
            '+' => {
                spec.sign = Sign::Plus;
                i += 1;
            }
            '-' => {
                spec.sign = Sign::Minus;
                i += 1;
            }
            ' ' => {
                spec.sign = Sign::Space;
                i += 1;
            }
            _ => {}
        }
    }

    // [#]
    if chars.get(i) == Some(&'#') {
        spec.alt = true;
        i += 1;
    }

    // [0] — zero-pad. Sets fill='0' and '=' align unless an align was explicit.
    if chars.get(i) == Some(&'0') {
        spec.zero = true;
        if spec.align.is_none() {
            spec.fill = '0';
            spec.align = Some(Align::Sign);
        }
        i += 1;
    }

    // [width]
    let (w, ni) = take_int(&chars, i);
    if let Some(w) = w {
        spec.width = Some(w);
    }
    i = ni;

    // [grouping]
    if let Some(&c) = chars.get(i) {
        if c == ',' || c == '_' {
            spec.grouping = Some(c);
            i += 1;
        }
    }

    // [.precision]
    if chars.get(i) == Some(&'.') {
        i += 1;
        let (p, ni) = take_int(&chars, i);
        match p {
            Some(p) => {
                spec.precision = Some(p);
                i = ni;
            }
            None => return Err(runtime_error("Format specifier missing precision")),
        }
    }

    // [type]
    if let Some(&c) = chars.get(i) {
        if matches!(c, 's' | 'd' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G' | 'x' | 'X' | 'o' | 'b' | '%' | 'n' | 'c') {
            spec.ty = Some(c);
            i += 1;
        }
    }

    if i != chars.len() {
        return Err(runtime_error(format!("Invalid format specifier '{s}'")));
    }
    Ok(spec)
}

/// Consume a run of ASCII digits starting at `i`, returning the value (if any)
/// and the new index.
fn take_int(chars: &[char], mut i: usize) -> (Option<usize>, usize) {
    let start = i;
    let mut n: usize = 0;
    while let Some(&c) = chars.get(i) {
        if c.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add((c as u8 - b'0') as usize);
            i += 1;
        } else {
            break;
        }
    }
    if i == start {
        (None, i)
    } else {
        (Some(n), i)
    }
}

// --- Applying a spec --------------------------------------------------------

fn apply(value: &Value, spec: &Spec) -> VResult<String> {
    match value {
        Value::Str(s) => format_str(&s.s, spec),
        Value::Bool(_) | Value::Int(_) | Value::Big(_) => format_int(value, spec),
        Value::Float(f) => format_float(*f, spec),
        other => Err(runtime_error(format!(
            "unsupported format string passed to {}.__format__",
            other.type_name()
        ))),
    }
}

fn format_str(s: &str, spec: &Spec) -> VResult<String> {
    match spec.ty {
        None | Some('s') => {}
        Some(t) => return Err(runtime_error(format!(
            "Unknown format code '{t}' for object of type 'str'"
        ))),
    }
    if spec.sign != Sign::Minus || spec.alt || spec.zero || spec.grouping.is_some() {
        return Err(runtime_error("invalid format spec for a string"));
    }
    // Precision truncates a string to that many characters.
    let mut body: String = match spec.precision {
        Some(p) => s.chars().take(p).collect(),
        None => s.to_string(),
    };
    // Strings default to left alignment.
    let align = spec.align.unwrap_or(Align::Left);
    body = pad(body, "", spec, align);
    Ok(body)
}

fn format_int(value: &Value, spec: &Spec) -> VResult<String> {
    // Types that reinterpret the integer as a float.
    if matches!(spec.ty, Some('f') | Some('F') | Some('e') | Some('E') | Some('g') | Some('G') | Some('%')) {
        let f = value_to_f64(value);
        return format_float(f, spec);
    }

    let (neg, digits) = int_digits(value);
    let base_ty = spec.ty.unwrap_or('d');

    if spec.precision.is_some() {
        return Err(runtime_error("Precision not allowed in integer format specifier"));
    }

    let (mut body, prefix) = match base_ty {
        'd' | 'n' => (group(&digits, spec.grouping, 3), String::new()),
        'x' => (group(&to_radix(&digits, 16, false), spec.grouping, 4), if spec.alt { "0x".into() } else { String::new() }),
        'X' => (group(&to_radix(&digits, 16, true), spec.grouping, 4), if spec.alt { "0X".into() } else { String::new() }),
        'o' => (group(&to_radix(&digits, 8, false), spec.grouping, 4), if spec.alt { "0o".into() } else { String::new() }),
        'b' => (group(&to_radix(&digits, 2, false), spec.grouping, 4), if spec.alt { "0b".into() } else { String::new() }),
        'c' => {
            let code = digits.parse::<u32>().map_err(|_| value_error("%c arg not in range"))?;
            let ch = char::from_u32(code)
                .ok_or_else(|| value_error("%c arg not in range(0x110000)"))?;
            return format_str(&ch.to_string(), &Spec { ty: None, ..copy_spec(spec) });
        }
        other => return Err(runtime_error(format!(
            "Unknown format code '{other}' for object of type 'int'"
        ))),
    };

    let sign = sign_str(neg, spec.sign);
    let head = format!("{sign}{prefix}");
    let align = spec.align.unwrap_or(Align::Right);
    body = pad(std::mem::take(&mut body), &head, spec, align);
    Ok(body)
}

fn format_float(f: f64, spec: &Spec) -> VResult<String> {
    if !f.is_finite() {
        let word = if f.is_nan() {
            "nan".to_string()
        } else {
            "inf".to_string()
        };
        let neg = f.is_sign_negative() && !f.is_nan();
        let word = match spec.ty {
            Some('E') | Some('F') | Some('G') => word.to_uppercase(),
            _ => word,
        };
        let sign = sign_str(neg, spec.sign);
        let align = spec.align.unwrap_or(Align::Right);
        // Zero-fill never applies to inf/nan.
        let spec = Spec { fill: if spec.zero { ' ' } else { spec.fill }, ..copy_spec(spec) };
        return Ok(pad(word, &sign, &spec, align));
    }

    let neg = f.is_sign_negative();
    let mag = f.abs();
    let ty = spec.ty;

    let mut digits = match ty {
        Some('f') | Some('F') => fixed(mag, spec.precision.unwrap_or(6)),
        Some('e') | Some('E') => sci(mag, spec.precision.unwrap_or(6), matches!(ty, Some('E'))),
        Some('%') => {
            let mut s = fixed(mag * 100.0, spec.precision.unwrap_or(6));
            s.push('%');
            s
        }
        Some('g') | Some('G') => general(mag, spec.precision, matches!(ty, Some('G')), spec.alt),
        None => match spec.precision {
            // No type + precision behaves like 'g' but keeps at least one
            // fractional digit / never strips to bare integer form.
            Some(p) => general(mag, Some(p.max(1)), false, spec.alt),
            None => default_float(mag),
        },
        Some(other) => return Err(runtime_error(format!(
            "Unknown format code '{other}' for object of type 'float'"
        ))),
    };

    // Thousands separators group the integer part only.
    if let Some(g) = spec.grouping {
        digits = group_float(&digits, g);
    }

    let sign = sign_str(neg, spec.sign);
    let align = spec.align.unwrap_or(Align::Right);
    Ok(pad(digits, &sign, spec, align))
}

// --- Numeric string helpers -------------------------------------------------

/// Decimal magnitude digits of an integer value, plus whether it was negative.
fn int_digits(value: &Value) -> (bool, String) {
    match value {
        Value::Bool(b) => (false, (*b as i64).to_string()),
        Value::Int(i) => {
            if *i < 0 {
                // Avoid overflow on i64::MIN by formatting through the string.
                let s = i.to_string();
                (true, s[1..].to_string())
            } else {
                (false, i.to_string())
            }
        }
        Value::Big(b) => {
            let s = b.to_string();
            match s.strip_prefix('-') {
                Some(rest) => (true, rest.to_string()),
                None => (false, s),
            }
        }
        _ => (false, "0".to_string()),
    }
}

fn value_to_f64(value: &Value) -> f64 {
    match value {
        Value::Bool(b) => *b as i64 as f64,
        Value::Int(i) => *i as f64,
        Value::Big(b) => b.to_f64(),
        Value::Float(f) => *f,
        _ => 0.0,
    }
}

/// Convert a non-negative decimal digit string to another radix.
fn to_radix(decimal: &str, radix: u32, upper: bool) -> String {
    // Fits in u128 for anything reasonable; fall back through BigInt-free long
    // division on the decimal string for very large magnitudes.
    if let Ok(n) = decimal.parse::<u128>() {
        if n == 0 {
            return "0".to_string();
        }
        let mut n = n;
        let mut out = Vec::new();
        while n > 0 {
            let d = (n % radix as u128) as u32;
            out.push(digit_char(d, upper));
            n /= radix as u128;
        }
        out.iter().rev().collect()
    } else {
        long_div_radix(decimal, radix, upper)
    }
}

/// Repeated long division of a decimal string — handles bignums without pulling
/// in the BigInt type (radix conversion is rare and not perf-critical).
fn long_div_radix(decimal: &str, radix: u32, upper: bool) -> String {
    let mut cur: Vec<u32> = decimal.chars().map(|c| c as u32 - '0' as u32).collect();
    let mut out = Vec::new();
    while !(cur.len() == 1 && cur[0] == 0) {
        let mut rem = 0u32;
        let mut next = Vec::with_capacity(cur.len());
        for &d in &cur {
            let acc = rem * 10 + d;
            next.push(acc / radix);
            rem = acc % radix;
        }
        // Drop leading zeros from the quotient.
        let first_nonzero = next.iter().position(|&d| d != 0).unwrap_or(next.len() - 1);
        cur = next[first_nonzero..].to_vec();
        out.push(digit_char(rem, upper));
    }
    out.iter().rev().collect()
}

fn digit_char(d: u32, upper: bool) -> char {
    if d < 10 {
        (b'0' + d as u8) as char
    } else if upper {
        (b'A' + (d - 10) as u8) as char
    } else {
        (b'a' + (d - 10) as u8) as char
    }
}

fn sign_str(neg: bool, sign: Sign) -> String {
    if neg {
        "-".to_string()
    } else {
        match sign {
            Sign::Plus => "+".to_string(),
            Sign::Space => " ".to_string(),
            Sign::Minus => String::new(),
        }
    }
}

/// Insert a grouping separator every `size` digits from the right.
fn group(digits: &str, sep: Option<char>, size: usize) -> String {
    let sep = match sep {
        Some(c) => c,
        None => return digits.to_string(),
    };
    let bytes: Vec<char> = digits.chars().collect();
    let mut out = String::new();
    let n = bytes.len();
    for (idx, ch) in bytes.iter().enumerate() {
        if idx > 0 && (n - idx).is_multiple_of(size) {
            out.push(sep);
        }
        out.push(*ch);
    }
    out
}

/// Group the integer portion of an already-formatted float string.
fn group_float(s: &str, sep: char) -> String {
    // Split off any exponent or trailing '%' first, then the fractional part.
    let (num, tail) = match s.find(['e', 'E']) {
        Some(p) => (&s[..p], &s[p..]),
        None => (s, ""),
    };
    let (num, pct) = match num.strip_suffix('%') {
        Some(rest) => (rest, "%"),
        None => (num, ""),
    };
    let (int_part, frac) = match num.find('.') {
        Some(p) => (&num[..p], &num[p..]),
        None => (num, ""),
    };
    format!("{}{}{}{}", group(int_part, Some(sep), 3), frac, pct, tail)
}

// --- Float rendering --------------------------------------------------------

fn fixed(mag: f64, prec: usize) -> String {
    format!("{mag:.prec$}")
}

fn sci(mag: f64, prec: usize, upper: bool) -> String {
    // Rust's `{:e}` gives `m.mmme<exp>` with no exponent sign/padding; rebuild
    // the exponent as Python does (sign + at least two digits).
    let raw = format!("{mag:.prec$e}");
    let (mantissa, exp) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    let esign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{e}{esign}{:02}", exp.abs())
}

fn general(mag: f64, precision: Option<usize>, upper: bool, alt: bool) -> String {
    let mut p = precision.unwrap_or(6);
    if p == 0 {
        p = 1;
    }
    // Determine the decimal exponent after rounding to p significant digits.
    let raw = format!("{mag:.*e}", p - 1);
    let exp: i32 = raw.split_once('e').and_then(|(_, e)| e.parse().ok()).unwrap_or(0);

    let mut out = if exp < -4 || exp >= p as i32 {
        sci(mag, p - 1, upper)
    } else {
        fixed(mag, (p as i32 - 1 - exp).max(0) as usize)
    };

    if !alt {
        out = strip_g_zeros(&out);
    }
    out
}

/// Strip trailing fractional zeros (and a dangling '.') from a 'g'/default
/// rendering, leaving any exponent suffix intact.
fn strip_g_zeros(s: &str) -> String {
    let (num, tail) = match s.find(['e', 'E']) {
        Some(p) => (&s[..p], &s[p..]),
        None => (s, ""),
    };
    if !num.contains('.') {
        return format!("{num}{tail}");
    }
    let trimmed = num.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}{tail}")
}

/// The no-type float rendering: CPython's `repr`/`str` shortest round-trip form
/// (always with a fractional part, e.g. `1.0`).
fn default_float(mag: f64) -> String {
    let s = format!("{mag}");
    if s.contains('.') || s.contains('e') || s.contains('E') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{s}.0")
    }
}

// --- Padding ----------------------------------------------------------------

/// Assemble `head` (sign/prefix) + `body`, then pad to `spec.width` under the
/// given alignment with `spec.fill`.
fn pad(body: String, head: &str, spec: &Spec, align: Align) -> String {
    let content_len = head.chars().count() + body.chars().count();
    let width = spec.width.unwrap_or(0);
    if content_len >= width {
        return format!("{head}{body}");
    }
    let padding = width - content_len;
    let fill = spec.fill;
    match align {
        Align::Left => format!("{head}{body}{}", repeat(fill, padding)),
        Align::Right => format!("{}{head}{body}", repeat(fill, padding)),
        Align::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{head}{body}{}", repeat(fill, left), repeat(fill, right))
        }
        // '=' — pad between the sign/prefix and the digits (numeric zero-fill).
        Align::Sign => format!("{head}{}{body}", repeat(fill, padding)),
    }
}

fn repeat(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

fn copy_spec(spec: &Spec) -> Spec {
    Spec {
        fill: spec.fill,
        align: spec.align,
        sign: spec.sign,
        alt: spec.alt,
        zero: spec.zero,
        width: spec.width,
        grouping: spec.grouping,
        precision: spec.precision,
        ty: spec.ty,
    }
}

/// A minimal `ascii()`-style escape of a repr string: escape non-ASCII as
/// `\xHH`/`\uHHHH`. Sufficient for `!a`, which the corpus does not exercise.
fn ascii_repr(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else if (c as u32) <= 0xff {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else if (c as u32) <= 0xffff {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push_str(&format!("\\U{:08x}", c as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Every expected value here was checked against CPython 3.12
    //! (`format(value, spec)` / f-strings), which is the ground truth the
    //! external corpus also uses.
    use super::*;
    use crate::bigint::BigInt;

    /// `format(value, spec)` with no conversion.
    fn fmt(v: &Value, spec: &str) -> String {
        format_value(v, CONV_NONE, spec).expect("format")
    }

    fn i(n: i64) -> Value {
        Value::Int(n)
    }
    fn f(x: f64) -> Value {
        Value::Float(x)
    }
    fn s(text: &str) -> Value {
        Value::str(text.to_string())
    }

    #[test]
    fn width_defaults_right_for_numbers_left_for_strings() {
        assert_eq!(fmt(&i(42), "5"), "   42");
        assert_eq!(fmt(&s("hi"), "5"), "hi   ");
    }

    #[test]
    fn alignment() {
        assert_eq!(fmt(&i(42), "<10"), "42        ");
        assert_eq!(fmt(&i(42), ">10"), "        42");
        assert_eq!(fmt(&i(42), "^10"), "    42    ");
        // Odd padding puts the extra fill on the right.
        assert_eq!(fmt(&i(42), "^11"), "    42     ");
        assert_eq!(fmt(&s("hi"), ">6"), "    hi");
        assert_eq!(fmt(&s("hi"), "^6"), "  hi  ");
    }

    #[test]
    fn custom_fill_char() {
        assert_eq!(fmt(&s("hi"), "*^6"), "**hi**");
        assert_eq!(fmt(&i(42), "*>8"), "******42");
        assert_eq!(fmt(&i(42), "0=8"), "00000042");
    }

    #[test]
    fn zero_padding() {
        assert_eq!(fmt(&i(42), "05d"), "00042");
        assert_eq!(fmt(&i(-42), "05d"), "-0042");
        assert_eq!(fmt(&i(42), "08"), "00000042");
        // Zero-fill floats keep the sign at the front.
        assert_eq!(fmt(&f(3.5), "08.2f"), "00003.50");
        assert_eq!(fmt(&f(-3.5), "08.2f"), "-0003.50");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(fmt(&i(1234567), ","), "1,234,567");
        assert_eq!(fmt(&i(1234567), "_"), "1_234_567");
        assert_eq!(fmt(&f(1234.5), ",.2f"), "1,234.50");
        assert_eq!(fmt(&i(255), "_x"), "ff");
        assert_eq!(fmt(&i(0xffff), "_x"), "ffff");
        assert_eq!(fmt(&i(0xfffff), "_x"), "f_ffff");
    }

    #[test]
    fn sign_options() {
        assert_eq!(fmt(&i(42), "+d"), "+42");
        assert_eq!(fmt(&i(42), " d"), " 42");
        assert_eq!(fmt(&i(-42), "+d"), "-42");
        assert_eq!(fmt(&f(8.14), "+.1f"), "+8.1");
    }

    #[test]
    fn float_precision() {
        assert_eq!(fmt(&f(8.75319), ".2f"), "8.75");
        assert_eq!(fmt(&f(8.75319), ".0f"), "9");
        assert_eq!(fmt(&f(8.75319), "10.3f"), "     8.753");
        // Banker's rounding, matching CPython.
        assert_eq!(fmt(&f(2.5), ".0f"), "2");
        assert_eq!(fmt(&f(0.5), ".0f"), "0");
        assert_eq!(fmt(&f(3.999), ".1f"), "4.0");
    }

    #[test]
    fn scientific_and_general() {
        assert_eq!(fmt(&f(1234.5), ".2e"), "1.23e+03");
        assert_eq!(fmt(&f(1234.5), ".2E"), "1.23E+03");
        assert_eq!(fmt(&f(0.00001234), "g"), "1.234e-05");
        assert_eq!(fmt(&f(1234.5), "g"), "1234.5");
        assert_eq!(fmt(&f(100.0), "g"), "100");
        assert_eq!(fmt(&f(123456789.0), "g"), "1.23457e+08");
    }

    #[test]
    fn radixes_and_percent() {
        assert_eq!(fmt(&i(255), "x"), "ff");
        assert_eq!(fmt(&i(255), "X"), "FF");
        assert_eq!(fmt(&i(255), "#x"), "0xff");
        assert_eq!(fmt(&i(255), "o"), "377");
        assert_eq!(fmt(&i(255), "#o"), "0o377");
        assert_eq!(fmt(&i(5), "b"), "101");
        assert_eq!(fmt(&i(5), "#b"), "0b101");
        assert_eq!(fmt(&i(255), "08x"), "000000ff");
        assert_eq!(fmt(&f(0.25), "%"), "25.000000%");
        assert_eq!(fmt(&f(0.25), ".1%"), "25.0%");
    }

    #[test]
    fn string_precision_truncates() {
        assert_eq!(fmt(&s("hello"), ".3"), "hel");
        assert_eq!(fmt(&s("hello"), "10.3"), "hel       ");
    }

    #[test]
    fn conversions() {
        assert_eq!(format_value(&s("hi"), CONV_REPR, "").unwrap(), "'hi'");
        assert_eq!(format_value(&s("hi"), CONV_STR, "").unwrap(), "hi");
        // A conversion applies before the spec: repr, then right-pad.
        assert_eq!(format_value(&s("hi"), CONV_REPR, ">6").unwrap(), "  'hi'");
    }

    #[test]
    fn bignum_formatting() {
        let big = Value::from_bigint(BigInt::parse_decimal("1180591620717411303424").unwrap());
        assert_eq!(fmt(&big, ","), "1,180,591,620,717,411,303,424");
        assert_eq!(fmt(&big, "x"), "400000000000000000");
    }

    #[test]
    fn empty_spec_is_str() {
        assert_eq!(fmt(&i(42), ""), "42");
        assert_eq!(fmt(&f(1.5), ""), "1.5");
        assert_eq!(fmt(&f(1.0), ""), "1.0");
    }

    #[test]
    fn errors() {
        // Precision is not allowed on integers.
        assert!(format_value(&i(42), CONV_NONE, ".2d").is_err());
        // A string cannot take a numeric type code.
        assert!(format_value(&s("x"), CONV_NONE, "d").is_err());
        // Trailing garbage in the spec is rejected.
        assert!(format_value(&i(1), CONV_NONE, "5zz").is_err());
    }
}
