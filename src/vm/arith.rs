//! Numeric and sequence operators for the VM.
//!
//! Integer arithmetic stays inline in `i64` and only promotes to [`BigInt`] on
//! genuine overflow (architecture point 4), via the `checked_*` operations.
//! Mixed-type arithmetic follows Python's tower: any `float` operand makes the
//! result a `float`; `/` is always true (float) division.

use crate::bigint::BigInt;
use crate::compiler::Op;
use crate::exc::{runtime_error, type_error, zero_division_error, VErr};
use crate::value::{Number, VResult, Value};

pub fn neg(v: &Value) -> VResult<Value> {
    match v.as_number() {
        Some(Number::Int(i)) => Ok(match i.checked_neg() {
            Some(n) => Value::Int(n),
            None => Value::from_bigint(BigInt::from_i64(i).neg()),
        }),
        Some(Number::Big(b)) => Ok(Value::from_bigint(b.neg())),
        Some(Number::Float(f)) => Ok(Value::Float(-f)),
        None => Err(type_error(format!("bad operand type for unary -: '{}'", v.type_name()))),
    }
}

pub fn pos(v: &Value) -> VResult<Value> {
    match v.as_number() {
        Some(Number::Int(i)) => Ok(Value::Int(i)),
        Some(Number::Big(b)) => Ok(Value::from_bigint(b)),
        Some(Number::Float(f)) => Ok(Value::Float(f)),
        None => Err(type_error(format!("bad operand type for unary +: '{}'", v.type_name()))),
    }
}

pub fn binary(op: &Op, a: &Value, b: &Value) -> VResult<Value> {
    match op {
        Op::BinAdd => add(a, b),
        Op::BinSub => num_only(a, b, "-", sub_num),
        Op::BinMul => mul(a, b),
        Op::BinDiv => num_only(a, b, "/", div_num),
        Op::BinFloorDiv => num_only(a, b, "//", floordiv_num),
        Op::BinMod => num_only(a, b, "%", mod_num),
        Op::BinPow => num_only(a, b, "**", pow_num),
        _ => unreachable!("binary called with a non-binary op"),
    }
}

/// Apply a numbers-only operator, producing a uniform type error otherwise.
fn num_only(
    a: &Value,
    b: &Value,
    sym: &str,
    f: fn(&Number, &Number) -> VResult<Value>,
) -> VResult<Value> {
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => f(&x, &y),
        _ => Err(type_err(sym, a, b)),
    }
}

fn type_err(sym: &str, a: &Value, b: &Value) -> VErr {
    type_error(format!(
        "unsupported operand type(s) for {}: '{}' and '{}'",
        sym,
        a.type_name(),
        b.type_name()
    ))
}

fn add(a: &Value, b: &Value) -> VResult<Value> {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        return add_num(&x, &y);
    }
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Ok(Value::str(format!("{}{}", x.s, y.s))),
        (Value::Bytes(x), Value::Bytes(y)) => {
            let mut v = Vec::with_capacity(x.len() + y.len());
            v.extend_from_slice(x);
            v.extend_from_slice(y);
            Ok(Value::bytes(v))
        }
        (Value::List(x), Value::List(y)) => {
            let mut v = x.borrow().clone();
            v.extend(y.borrow().iter().cloned());
            Ok(Value::List(crate::value::OroList::new(v)))
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            let mut v = (**x).clone();
            v.extend(y.iter().cloned());
            Ok(Value::Tuple(crate::value::OroTuple::new(v)))
        }
        _ => Err(type_err("+", a, b)),
    }
}

fn mul(a: &Value, b: &Value) -> VResult<Value> {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        return mul_num(&x, &y);
    }
    // Sequence repetition: `seq * int` in either order. Exactly one operand is
    // the integer count and the other the sequence.
    let (seq, count) = if let Some(n) = int_count(a) {
        (b, n)
    } else if let Some(n) = int_count(b) {
        (a, n)
    } else {
        return Err(type_err("*", a, b));
    };
    let count = count.max(0) as usize;
    match seq {
        Value::Str(s) => Ok(Value::str(s.s.repeat(count))),
        Value::Bytes(b) => Ok(Value::bytes(b.repeat(count))),
        Value::List(l) => {
            let base = l.borrow();
            let mut out = Vec::with_capacity(base.len() * count);
            for _ in 0..count {
                out.extend(base.iter().cloned());
            }
            Ok(Value::List(crate::value::OroList::new(out)))
        }
        Value::Tuple(t) => {
            let mut out = Vec::with_capacity(t.len() * count);
            for _ in 0..count {
                out.extend(t.iter().cloned());
            }
            Ok(Value::Tuple(crate::value::OroTuple::new(out)))
        }
        _ => Err(type_err("*", a, b)),
    }
}

/// The integer value of a `bool`/`int` operand, or `None` for anything else.
fn int_count(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

// --- Numeric core -----------------------------------------------------------

fn add_num(a: &Number, b: &Number) -> VResult<Value> {
    if a.is_float() || b.is_float() {
        return Ok(Value::Float(a.to_f64() + b.to_f64()));
    }
    if let (Number::Int(x), Number::Int(y)) = (a, b) {
        return Ok(match x.checked_add(*y) {
            Some(v) => Value::Int(v),
            None => Value::from_bigint(BigInt::from_i64(*x).add(&BigInt::from_i64(*y))),
        });
    }
    Ok(Value::from_bigint(a.to_bigint().add(&b.to_bigint())))
}

fn sub_num(a: &Number, b: &Number) -> VResult<Value> {
    if a.is_float() || b.is_float() {
        return Ok(Value::Float(a.to_f64() - b.to_f64()));
    }
    if let (Number::Int(x), Number::Int(y)) = (a, b) {
        return Ok(match x.checked_sub(*y) {
            Some(v) => Value::Int(v),
            None => Value::from_bigint(BigInt::from_i64(*x).sub(&BigInt::from_i64(*y))),
        });
    }
    Ok(Value::from_bigint(a.to_bigint().sub(&b.to_bigint())))
}

fn mul_num(a: &Number, b: &Number) -> VResult<Value> {
    if a.is_float() || b.is_float() {
        return Ok(Value::Float(a.to_f64() * b.to_f64()));
    }
    if let (Number::Int(x), Number::Int(y)) = (a, b) {
        return Ok(match x.checked_mul(*y) {
            Some(v) => Value::Int(v),
            None => Value::from_bigint(BigInt::from_i64(*x).mul(&BigInt::from_i64(*y))),
        });
    }
    Ok(Value::from_bigint(a.to_bigint().mul(&b.to_bigint())))
}

/// True division: always a float, matching Python 3.
fn div_num(a: &Number, b: &Number) -> VResult<Value> {
    let d = b.to_f64();
    if d == 0.0 {
        return Err(zero_division_error("division by zero"));
    }
    Ok(Value::Float(a.to_f64() / d))
}

fn floordiv_num(a: &Number, b: &Number) -> VResult<Value> {
    if a.is_float() || b.is_float() {
        let d = b.to_f64();
        if d == 0.0 {
            return Err(zero_division_error("float floor division by zero"));
        }
        return Ok(Value::Float((a.to_f64() / d).floor()));
    }
    if let (Number::Int(x), Number::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(zero_division_error("integer division or modulo by zero"));
        }
        // Guard the one overflow case (i64::MIN // -1) by falling through to
        // BigInt.
        if let Some(q) = floordiv_i64(*x, *y) {
            return Ok(Value::Int(q));
        }
    }
    match a.to_bigint().divmod_floor(&b.to_bigint()) {
        Some((q, _)) => Ok(Value::from_bigint(q)),
        None => Err(zero_division_error("integer division or modulo by zero")),
    }
}

fn mod_num(a: &Number, b: &Number) -> VResult<Value> {
    if a.is_float() || b.is_float() {
        let d = b.to_f64();
        if d == 0.0 {
            return Err(zero_division_error("float modulo by zero"));
        }
        let r = a.to_f64() - (a.to_f64() / d).floor() * d;
        return Ok(Value::Float(r));
    }
    if let (Number::Int(x), Number::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(zero_division_error("integer division or modulo by zero"));
        }
        if let Some(r) = mod_i64(*x, *y) {
            return Ok(Value::Int(r));
        }
    }
    match a.to_bigint().divmod_floor(&b.to_bigint()) {
        Some((_, r)) => Ok(Value::from_bigint(r)),
        None => Err(zero_division_error("integer division or modulo by zero")),
    }
}

fn pow_num(a: &Number, b: &Number) -> VResult<Value> {
    // A negative or float exponent gives a float, as in Python.
    if a.is_float() || b.is_float() {
        return Ok(Value::Float(a.to_f64().powf(b.to_f64())));
    }
    let exp = match b {
        Number::Int(e) => *e,
        // A bignum exponent is astronomically large; refuse rather than hang.
        Number::Big(_) => return Err(runtime_error("exponent too large")),
        Number::Float(_) => unreachable!(),
    };
    if exp < 0 {
        return Ok(Value::Float(a.to_f64().powf(exp as f64)));
    }
    Ok(Value::from_bigint(a.to_bigint().pow_u64(exp as u64)))
}

/// Python floor division for `i64`, or `None` on the `i64::MIN / -1` overflow.
fn floordiv_i64(x: i64, y: i64) -> Option<i64> {
    let q = x.checked_div(y)?;
    let r = x.checked_rem(y)?;
    if r != 0 && ((r < 0) != (y < 0)) {
        q.checked_sub(1)
    } else {
        Some(q)
    }
}

/// Python modulo for `i64` (result takes the divisor's sign).
fn mod_i64(x: i64, y: i64) -> Option<i64> {
    let r = x.checked_rem(y)?;
    if r != 0 && ((r < 0) != (y < 0)) {
        r.checked_add(y)
    } else {
        Some(r)
    }
}
