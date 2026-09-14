//! A minimal arbitrary-precision signed integer.
//!
//! Oro stores ordinary integers inline as `i64` (see [`crate::value::Value`]);
//! this type exists only for the values that overflow `i64`. Architecture point
//! 4 is deliberate: we never allocate for integers that fit inline, so `BigInt`
//! is reached only through the `checked_*` overflow path.
//!
//! Representation: sign-magnitude, magnitude in base 2^32 little-endian with no
//! trailing zero limbs (so the magnitude of zero is the empty vector). This is
//! kept intentionally simple — correctness over raw speed — and depends on
//! nothing outside `std`, matching the crate's zero-dependency rule.

use std::cmp::Ordering;
use std::fmt;

/// A sign-magnitude arbitrary-precision integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BigInt {
    /// True when the value is strictly negative. Zero is always non-negative.
    negative: bool,
    /// Base-2^32 limbs, least significant first, no trailing zero limbs.
    mag: Vec<u32>,
}

impl BigInt {
    /// The additive identity.
    pub fn zero() -> BigInt {
        BigInt {
            negative: false,
            mag: Vec::new(),
        }
    }

    /// Build from an `i64`, handling `i64::MIN` without overflow.
    pub fn from_i64(v: i64) -> BigInt {
        let negative = v < 0;
        // `unsigned_abs` gives the magnitude even for i64::MIN.
        let m = v.unsigned_abs();
        let mut mag = Vec::new();
        if m != 0 {
            mag.push((m & 0xFFFF_FFFF) as u32);
            let hi = (m >> 32) as u32;
            if hi != 0 {
                mag.push(hi);
            }
        }
        BigInt { negative, mag }
    }

    /// Parse a non-empty run of decimal digits (no sign, no separators). Returns
    /// `None` if any character is not an ASCII digit.
    pub fn parse_decimal(s: &str) -> Option<BigInt> {
        if s.is_empty() {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        for ch in s.bytes() {
            if !ch.is_ascii_digit() {
                return None;
            }
            let d = (ch - b'0') as u32;
            mul_small_inplace(&mut mag, 10);
            add_small_inplace(&mut mag, d);
        }
        normalize(&mut mag);
        Some(BigInt {
            negative: false,
            mag,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// If the value fits in an `i64`, return it. Used to demote results back to
    /// the inline representation so the "a `BigInt` is always outside `i64`
    /// range" invariant holds everywhere else.
    pub fn to_i64(&self) -> Option<i64> {
        match self.mag.len() {
            0 => Some(0),
            1 => {
                let v = self.mag[0] as i64;
                Some(if self.negative { -v } else { v })
            }
            2 => {
                let m = (self.mag[0] as u64) | ((self.mag[1] as u64) << 32);
                if self.negative {
                    // i64::MIN has magnitude 2^63.
                    if m <= (i64::MAX as u64) + 1 {
                        Some((m as i64).wrapping_neg())
                    } else {
                        None
                    }
                } else if m <= i64::MAX as u64 {
                    Some(m as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Lossy conversion to `f64` (may be `inf` for very large magnitudes).
    pub fn to_f64(&self) -> f64 {
        let mut acc = 0.0f64;
        for &limb in self.mag.iter().rev() {
            acc = acc * 4294967296.0 + limb as f64;
        }
        if self.negative {
            -acc
        } else {
            acc
        }
    }

    pub fn neg(&self) -> BigInt {
        if self.is_zero() {
            self.clone()
        } else {
            BigInt {
                negative: !self.negative,
                mag: self.mag.clone(),
            }
        }
    }

    pub fn abs(&self) -> BigInt {
        BigInt {
            negative: false,
            mag: self.mag.clone(),
        }
    }

    pub fn add(&self, other: &BigInt) -> BigInt {
        if self.negative == other.negative {
            BigInt {
                negative: self.negative,
                mag: add_mag(&self.mag, &other.mag),
            }
            .normalized()
        } else {
            // Different signs: subtract the smaller magnitude from the larger.
            match cmp_mag(&self.mag, &other.mag) {
                Ordering::Equal => BigInt::zero(),
                Ordering::Greater => BigInt {
                    negative: self.negative,
                    mag: sub_mag(&self.mag, &other.mag),
                }
                .normalized(),
                Ordering::Less => BigInt {
                    negative: other.negative,
                    mag: sub_mag(&other.mag, &self.mag),
                }
                .normalized(),
            }
        }
    }

    pub fn sub(&self, other: &BigInt) -> BigInt {
        self.add(&other.neg())
    }

    pub fn mul(&self, other: &BigInt) -> BigInt {
        if self.is_zero() || other.is_zero() {
            return BigInt::zero();
        }
        BigInt {
            negative: self.negative != other.negative,
            mag: mul_mag(&self.mag, &other.mag),
        }
        .normalized()
    }

    /// Python-style floor division and modulo together. Returns `None` when
    /// `other` is zero. The remainder takes the sign of the divisor and the
    /// quotient is floored toward negative infinity, matching Oro/Python `//`
    /// and `%`.
    pub fn divmod_floor(&self, other: &BigInt) -> Option<(BigInt, BigInt)> {
        if other.is_zero() {
            return None;
        }
        let (q_mag, r_mag) = divmod_mag(&self.mag, &other.mag);
        // Truncated (toward-zero) quotient/remainder in sign-magnitude form.
        let q_neg = self.negative != other.negative;
        let mut q = BigInt {
            negative: q_neg,
            mag: q_mag,
        }
        .normalized();
        let mut r = BigInt {
            negative: self.negative,
            mag: r_mag,
        }
        .normalized();

        // Adjust from truncation toward zero to flooring toward -inf: when the
        // remainder is non-zero and its sign differs from the divisor's, nudge.
        if !r.is_zero() && (r.negative != other.negative) {
            q = q.sub(&BigInt::from_i64(1));
            r = r.add(other);
        }
        Some((q, r))
    }

    /// `self ** exp` for a non-negative `exp`, by exponentiation by squaring.
    pub fn pow_u64(&self, mut exp: u64) -> BigInt {
        let mut result = BigInt::from_i64(1);
        let mut base = self.clone();
        while exp > 0 {
            if exp & 1 == 1 {
                result = result.mul(&base);
            }
            exp >>= 1;
            if exp > 0 {
                base = base.mul(&base);
            }
        }
        result
    }

    fn normalized(mut self) -> BigInt {
        normalize(&mut self.mag);
        if self.mag.is_empty() {
            self.negative = false;
        }
        self
    }

    /// `~self`, which is `-self - 1` — the same identity the inline `i64` path
    /// relies on, and the definition of the operator rather than a consequence
    /// of some representation.
    pub fn not(&self) -> BigInt {
        self.neg().sub(&BigInt::from_i64(1))
    }

    pub fn bitand(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a & b, self.negative && other.negative)
    }

    pub fn bitor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a | b, self.negative || other.negative)
    }

    pub fn bitxor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a ^ b, self.negative != other.negative)
    }

    /// The shared body of `&`, `|` and `^`.
    ///
    /// Python's integers are conceptually **infinite two's-complement**: a
    /// negative value is its magnitude's complement under an endless run of
    /// sign bits. This type is sign-magnitude, so each operand is widened into
    /// two's-complement limbs over a common width, combined limb by limb, and
    /// converted back. One limb beyond the longer magnitude is enough width:
    /// it is the limb that holds the sign, and it is all-zero for a
    /// non-negative value and all-ones for a negative one, so every limb above
    /// it would repeat.
    ///
    /// `negative` is the result's sign bit, which is the operator applied to
    /// the two sign bits — the caller computes it because it is the one part
    /// that is not a limb.
    fn bitwise(&self, other: &BigInt, f: fn(u32, u32) -> u32, negative: bool) -> BigInt {
        let n = self.mag.len().max(other.mag.len()) + 1;
        let (x, y) = (self.twos(n), other.twos(n));
        let limbs: Vec<u32> = x.iter().zip(&y).map(|(&a, &b)| f(a, b)).collect();
        from_twos(limbs, negative)
    }

    /// This value as `n` two's-complement limbs (`n` at least one more than
    /// the magnitude's length, so the top limb is the sign).
    fn twos(&self, n: usize) -> Vec<u32> {
        let mut out = vec![0u32; n];
        if !self.negative {
            out[..self.mag.len()].copy_from_slice(&self.mag);
            return out;
        }
        // -m is !(m - 1). The magnitude is non-empty here, because zero is
        // never negative.
        let mut m = self.mag.clone();
        sub_one(&mut m);
        for (i, limb) in out.iter_mut().enumerate() {
            *limb = !m.get(i).copied().unwrap_or(0);
        }
        out
    }

    /// `self << n`. The magnitude shifts and the sign is untouched, which is
    /// what Python means by it: `-1 << 3` is -8.
    pub fn shl(&self, n: u64) -> BigInt {
        if self.is_zero() {
            return BigInt::zero();
        }
        let whole = (n / 32) as usize;
        let bits = (n % 32) as u32;
        let mut mag = vec![0u32; whole];
        if bits == 0 {
            mag.extend_from_slice(&self.mag);
        } else {
            let mut carry = 0u32;
            for &limb in &self.mag {
                mag.push((limb << bits) | carry);
                carry = limb >> (32 - bits);
            }
            if carry != 0 {
                mag.push(carry);
            }
        }
        normalize(&mut mag);
        BigInt {
            negative: self.negative,
            mag,
        }
    }

    /// `self >> n`, which Python defines as `self // 2**n` — floored, so a
    /// negative value rounds *away* from zero: `-5 >> 1` is -3, not -2.
    pub fn shr(&self, n: u64) -> BigInt {
        let whole = (n / 32) as usize;
        let bits = (n % 32) as u32;
        if whole >= self.mag.len() {
            // Everything shifted out. Flooring makes that -1 for a negative
            // value and 0 for a non-negative one.
            return if self.negative {
                BigInt::from_i64(-1)
            } else {
                BigInt::zero()
            };
        }
        // Whether a 1 bit fell off the bottom, which is what decides the floor
        // adjustment below.
        let mut lost = self.mag[..whole].iter().any(|&limb| limb != 0);
        let src = &self.mag[whole..];
        let mut mag = Vec::with_capacity(src.len());
        if bits == 0 {
            mag.extend_from_slice(src);
        } else {
            lost = lost || src[0] & ((1u32 << bits) - 1) != 0;
            for i in 0..src.len() {
                let hi = src.get(i + 1).copied().unwrap_or(0);
                mag.push((src[i] >> bits) | (hi << (32 - bits)));
            }
        }
        normalize(&mut mag);
        let out = BigInt {
            negative: self.negative,
            mag,
        }
        .normalized();
        if self.negative && lost {
            // Truncation toward zero gave -(m >> n); the floor is one below it.
            out.sub(&BigInt::from_i64(1))
        } else {
            out
        }
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &other.mag),
            (true, true) => cmp_mag(&other.mag, &self.mag),
        }
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_str("0");
        }
        // Repeatedly divide the magnitude by 1e9, emitting 9 decimal digits at a
        // time, then reverse.
        let mut mag = self.mag.clone();
        let mut chunks: Vec<u32> = Vec::new();
        while !mag.is_empty() {
            let rem = divmod_small_inplace(&mut mag, 1_000_000_000);
            chunks.push(rem);
        }
        let mut out = String::new();
        if self.negative {
            out.push('-');
        }
        // Most significant chunk without leading zeros; the rest zero-padded.
        if let Some((first, rest)) = chunks.split_last() {
            out.push_str(&first.to_string());
            for chunk in rest.iter().rev() {
                out.push_str(&format!("{chunk:09}"));
            }
        }
        f.write_str(&out)
    }
}

// --- Magnitude primitives ---------------------------------------------------

fn normalize(mag: &mut Vec<u32>) {
    while let Some(&0) = mag.last() {
        mag.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    match a.len().cmp(&b.len()) {
        Ordering::Equal => {
            for i in (0..a.len()).rev() {
                match a[i].cmp(&b[i]) {
                    Ordering::Equal => continue,
                    ord => return ord,
                }
            }
            Ordering::Equal
        }
        ord => ord,
    }
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    let n = a.len().max(b.len());
    for i in 0..n {
        let av = *a.get(i).unwrap_or(&0) as u64;
        let bv = *b.get(i).unwrap_or(&0) as u64;
        let sum = av + bv + carry;
        out.push((sum & 0xFFFF_FFFF) as u32);
        carry = sum >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`, requiring `a >= b` (magnitude compare). Result is normalized.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &limb) in a.iter().enumerate() {
        let av = limb as i64;
        let bv = *b.get(i).unwrap_or(&0) as i64;
        let mut diff = av - bv - borrow;
        if diff < 0 {
            diff += 1 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(diff as u32);
    }
    normalize(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &av) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &bv) in b.iter().enumerate() {
            let idx = i + j;
            let cur = out[idx] as u64 + av as u64 * bv as u64 + carry;
            out[idx] = (cur & 0xFFFF_FFFF) as u32;
            carry = cur >> 32;
        }
        let mut idx = i + b.len();
        while carry != 0 {
            let cur = out[idx] as u64 + carry;
            out[idx] = (cur & 0xFFFF_FFFF) as u32;
            carry = cur >> 32;
            idx += 1;
        }
    }
    normalize(&mut out);
    out
}

fn mul_small_inplace(mag: &mut Vec<u32>, factor: u32) {
    let mut carry = 0u64;
    for limb in mag.iter_mut() {
        let cur = *limb as u64 * factor as u64 + carry;
        *limb = (cur & 0xFFFF_FFFF) as u32;
        carry = cur >> 32;
    }
    while carry != 0 {
        mag.push((carry & 0xFFFF_FFFF) as u32);
        carry >>= 32;
    }
}

fn add_small_inplace(mag: &mut Vec<u32>, addend: u32) {
    let mut carry = addend as u64;
    let mut i = 0;
    while carry != 0 {
        if i == mag.len() {
            mag.push(0);
        }
        let cur = mag[i] as u64 + carry;
        mag[i] = (cur & 0xFFFF_FFFF) as u32;
        carry = cur >> 32;
        i += 1;
    }
}

/// Subtract one from a non-zero magnitude, in place.
fn sub_one(mag: &mut Vec<u32>) {
    for limb in mag.iter_mut() {
        if *limb == 0 {
            *limb = u32::MAX; // borrow from the next limb
        } else {
            *limb -= 1;
            break;
        }
    }
    normalize(mag);
}

/// Read two's-complement limbs back as a sign-magnitude value. `negative` is
/// the sign the caller derived from the operands' sign bits.
fn from_twos(limbs: Vec<u32>, negative: bool) -> BigInt {
    let mut mag = limbs;
    if negative {
        // The magnitude of a negative two's-complement value is !limbs + 1.
        for limb in mag.iter_mut() {
            *limb = !*limb;
        }
        add_small_inplace(&mut mag, 1);
    }
    normalize(&mut mag);
    BigInt { negative, mag }.normalized()
}

/// Divide the magnitude in place by a small divisor, returning the remainder.
fn divmod_small_inplace(mag: &mut Vec<u32>, divisor: u32) -> u32 {
    let mut rem = 0u64;
    for limb in mag.iter_mut().rev() {
        let cur = (rem << 32) | *limb as u64;
        *limb = (cur / divisor as u64) as u32;
        rem = cur % divisor as u64;
    }
    normalize(mag);
    rem as u32
}

/// Long division of magnitudes via bit-at-a-time shifting. Returns
/// `(quotient, remainder)` magnitudes. Simple and correct for any size; not the
/// fastest algorithm, but division is off the hot path.
fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    debug_assert!(!b.is_empty(), "division by zero magnitude");
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    let bits = a.len() * 32;
    let mut q = vec![0u32; a.len()];
    let mut r: Vec<u32> = Vec::new();
    for i in (0..bits).rev() {
        shl1(&mut r);
        if bit(a, i) {
            if r.is_empty() {
                r.push(1);
            } else {
                r[0] |= 1;
            }
        }
        if cmp_mag(&r, b) != Ordering::Less {
            r = sub_mag(&r, b);
            set_bit(&mut q, i);
        }
    }
    normalize(&mut q);
    normalize(&mut r);
    (q, r)
}

fn bit(mag: &[u32], i: usize) -> bool {
    let limb = i / 32;
    let off = i % 32;
    mag.get(limb).map(|&l| (l >> off) & 1 == 1).unwrap_or(false)
}

fn set_bit(mag: &mut Vec<u32>, i: usize) {
    let limb = i / 32;
    let off = i % 32;
    if limb >= mag.len() {
        mag.resize(limb + 1, 0);
    }
    mag[limb] |= 1 << off;
}

/// Shift a magnitude left by one bit, in place.
fn shl1(mag: &mut Vec<u32>) {
    let mut carry = 0u32;
    for limb in mag.iter_mut() {
        let new_carry = *limb >> 31;
        *limb = (*limb << 1) | carry;
        carry = new_carry;
    }
    if carry != 0 {
        mag.push(carry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(s: &str) -> BigInt {
        if let Some(rest) = s.strip_prefix('-') {
            BigInt::parse_decimal(rest).unwrap().neg()
        } else {
            BigInt::parse_decimal(s).unwrap()
        }
    }

    #[test]
    fn parse_and_display_roundtrip() {
        for s in [
            "0",
            "1",
            "9",
            "10",
            "4294967296",
            "123456789012345678901234567890",
        ] {
            assert_eq!(big(s).to_string(), s);
        }
    }

    #[test]
    fn from_i64_edges() {
        assert_eq!(BigInt::from_i64(0).to_string(), "0");
        assert_eq!(BigInt::from_i64(i64::MAX).to_string(), i64::MAX.to_string());
        assert_eq!(BigInt::from_i64(i64::MIN).to_string(), i64::MIN.to_string());
        assert_eq!(BigInt::from_i64(i64::MIN).to_i64(), Some(i64::MIN));
        assert_eq!(BigInt::from_i64(i64::MAX).to_i64(), Some(i64::MAX));
    }

    #[test]
    fn add_sub_mul() {
        assert_eq!(
            big("999999999999999999").add(&big("1")).to_string(),
            "1000000000000000000"
        );
        assert_eq!(
            big("1000000000000000000").sub(&big("1")).to_string(),
            "999999999999999999"
        );
        assert_eq!(big("-5").add(&big("3")).to_string(), "-2");
        assert_eq!(big("5").add(&big("-8")).to_string(), "-3");
        let f = big("100000000000000000000");
        assert_eq!(
            f.mul(&f).to_string(),
            "10000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn factorial_25_overflows_i64() {
        let mut acc = BigInt::from_i64(1);
        for n in 2..=25i64 {
            acc = acc.mul(&BigInt::from_i64(n));
        }
        assert_eq!(acc.to_string(), "15511210043330985984000000");
        assert!(acc.to_i64().is_none());
    }

    #[test]
    fn divmod_floor_semantics() {
        // Positive.
        let (q, r) = big("100").divmod_floor(&big("7")).unwrap();
        assert_eq!((q.to_string(), r.to_string()), ("14".into(), "2".into()));
        // Negative dividend floors toward -inf; remainder takes divisor sign.
        let (q, r) = big("-7").divmod_floor(&big("3")).unwrap();
        assert_eq!((q.to_string(), r.to_string()), ("-3".into(), "2".into()));
        let (q, r) = big("7").divmod_floor(&big("-3")).unwrap();
        assert_eq!((q.to_string(), r.to_string()), ("-3".into(), "-2".into()));
        assert!(big("1").divmod_floor(&BigInt::zero()).is_none());
    }

    #[test]
    fn pow_and_demote() {
        assert_eq!(BigInt::from_i64(2).pow_u64(10).to_i64(), Some(1024));
        assert_eq!(
            BigInt::from_i64(2).pow_u64(64).to_string(),
            "18446744073709551616"
        );
    }

    #[test]
    fn ordering() {
        assert!(big("-1000000000000000000000") < big("5"));
        assert!(big("123456789012345678901") > big("123456789012345678900"));
        assert!(big("-5") < big("-4"));
    }
}
