//! Exact decimal numbers for JSON Schema's numeric keywords.
//!
//! JSON numbers are decimal text; converting them to `f64` would make
//! `minimum`, `maximum`, `multipleOf`, `enum` and `uniqueItems` round (`0.3` is
//! not a multiple of `0.1` in binary floating point, and two integers beyond
//! 2^53 compare equal). A [`Decimal`] keeps the exact value as an integer
//! mantissa and a power-of-ten exponent, normalized so that every value has
//! one representation: comparison and equality are exact for every number
//! whose exponent (after normalization) is within ±2^52. Text beyond that
//! range is not a `Decimal` at all ([`Decimal::parse`] returns `None`), and
//! the validator refuses an instance holding such a number rather than
//! compare it inexactly.

use core::cmp::Ordering;

/// The largest exponent magnitude a `Decimal` holds: far beyond any number a
/// real document holds, and small enough that exponent arithmetic on two of
/// them cannot overflow an `i64`.
const EXPONENT_LIMIT: i64 = 1 << 52;

/// The most mantissa digits a `multipleOf` divisor may have: it must fit a
/// `u64`, so the remainder arithmetic stays within `u128`.
pub(crate) const MAX_DIVISOR_DIGITS: usize = 19;

/// An exact decimal: `(-1)^negative * digits * 10^exponent`.
///
/// Normalized: `digits` has no leading or trailing zero (ASCII digits), and
/// zero is the empty mantissa with exponent 0 and `negative` false, so `-0`,
/// `0.0` and `0e5` are all the same value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decimal {
    negative: bool,
    digits: Box<[u8]>,
    exponent: i64,
}

impl Decimal {
    /// Reads a JSON number's text. `None` for text that is not a JSON number
    /// (jsonx-lib's numbers always are), and for a number whose normalized
    /// exponent is beyond ±2^52.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut at = 0;
        let negative = bytes.first() == Some(&b'-');
        if negative {
            at += 1;
        }
        let mut mantissa = Vec::with_capacity(bytes.len());
        let int_start = at;
        while let Some(b) = bytes.get(at).filter(|b| b.is_ascii_digit()) {
            mantissa.push(*b);
            at += 1;
        }
        if at == int_start {
            return None;
        }
        let mut exponent: i64 = 0;
        if bytes.get(at) == Some(&b'.') {
            at += 1;
            let frac_start = at;
            while let Some(b) = bytes.get(at).filter(|b| b.is_ascii_digit()) {
                mantissa.push(*b);
                exponent = exponent.saturating_sub(1);
                at += 1;
            }
            if at == frac_start {
                return None;
            }
        }
        if matches!(bytes.get(at), Some(b'e' | b'E')) {
            at += 1;
            let sign: i64 = match bytes.get(at) {
                Some(b'-') => {
                    at += 1;
                    -1
                }
                Some(b'+') => {
                    at += 1;
                    1
                }
                _ => 1,
            };
            let exp_start = at;
            let mut written: i64 = 0;
            while let Some(b) = bytes.get(at).filter(|b| b.is_ascii_digit()) {
                // Saturates far above the limit, which is checked below.
                written = written
                    .saturating_mul(10)
                    .saturating_add(i64::from(*b - b'0'))
                    .min(EXPONENT_LIMIT * 4);
                at += 1;
            }
            if at == exp_start {
                return None;
            }
            exponent = exponent.saturating_add(sign * written);
        }
        if at != bytes.len() {
            return None;
        }
        Self::normalized(negative, mantissa, exponent)
    }

    fn normalized(negative: bool, mut mantissa: Vec<u8>, mut exponent: i64) -> Option<Self> {
        let leading = mantissa.iter().take_while(|&&d| d == b'0').count();
        mantissa.drain(..leading).for_each(drop);
        while mantissa.last() == Some(&b'0') {
            let _trailing_zero = mantissa.pop();
            exponent = exponent.saturating_add(1);
        }
        if mantissa.is_empty() {
            return Some(Self::zero());
        }
        if !(-EXPONENT_LIMIT..=EXPONENT_LIMIT).contains(&exponent) {
            return None;
        }
        Some(Self {
            negative,
            digits: mantissa.into_boxed_slice(),
            exponent,
        })
    }

    fn zero() -> Self {
        Self {
            negative: false,
            digits: Box::new([]),
            exponent: 0,
        }
    }

    /// Whether the value is zero.
    pub(crate) fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// Whether the value is negative (zero is not).
    pub(crate) fn is_negative(&self) -> bool {
        self.negative
    }

    /// Whether the value is a whole number (JSON Schema's `integer`: `1.0` is
    /// one).
    pub(crate) fn is_integer(&self) -> bool {
        self.is_zero() || self.exponent >= 0
    }

    /// The mantissa's digit count.
    pub(crate) fn mantissa_digits(&self) -> usize {
        self.digits.len()
    }

    /// The value as a `u64` when it is a non-negative integer, saturating at
    /// `u64::MAX` for larger ones. `None` for negative or fractional values.
    pub(crate) fn to_u64_saturating(&self) -> Option<u64> {
        if self.negative || !self.is_integer() {
            return None;
        }
        let total = usize::try_from(self.exponent)
            .ok()
            .and_then(|e| e.checked_add(self.digits.len()))
            .unwrap_or(usize::MAX);
        if total > 20 {
            return Some(u64::MAX);
        }
        let mut value: u64 = 0;
        for &d in self.digits.iter() {
            value = match value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(d - b'0')))
            {
                Some(v) => v,
                None => return Some(u64::MAX),
            };
        }
        for _ in 0..self.exponent {
            value = match value.checked_mul(10) {
                Some(v) => v,
                None => return Some(u64::MAX),
            };
        }
        Some(value)
    }

    /// The unsigned mantissa as a `u64`, if it has at most
    /// [`MAX_DIVISOR_DIGITS`] digits and fits.
    fn mantissa_u64(&self) -> Option<u64> {
        if self.digits.len() > MAX_DIVISOR_DIGITS {
            return None;
        }
        let mut value: u64 = 0;
        for &d in self.digits.iter() {
            value = value.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
        }
        Some(value)
    }

    /// Whether `self` is an integer multiple of `divisor` (a positive value
    /// whose mantissa fits a `u64`; the schema compiler refuses others).
    /// Exact: `0.3` is a multiple of `0.1`.
    pub(crate) fn is_multiple_of(&self, divisor: &Decimal) -> bool {
        if self.is_zero() {
            return true;
        }
        let Some(modulus) = divisor.mantissa_u64().filter(|&m| m > 0) else {
            return false;
        };
        // self / divisor = (m_self / m_div) * 10^(e_self - e_div). With
        // e_self < e_div, the quotient is an integer only if m_self ends in a
        // zero, which a normalized mantissa never does.
        let Some(shift) = self.exponent.checked_sub(divisor.exponent) else {
            return false;
        };
        if shift < 0 {
            return false;
        }
        let modulus = u128::from(modulus);
        let mut remainder: u128 = 0;
        for &d in self.digits.iter() {
            remainder = (remainder * 10 + u128::from(d - b'0')) % modulus;
        }
        if remainder == 0 {
            return true;
        }
        let power = pow10_mod(shift.unsigned_abs(), modulus);
        (remainder * power) % modulus == 0
    }

    /// Appends the canonical text: equal values write equal text, so the
    /// text can stand for the value in sets and comparisons.
    pub(crate) fn write_canonical(&self, out: &mut String) {
        if self.is_zero() {
            out.push('0');
            return;
        }
        if self.negative {
            out.push('-');
        }
        for &d in self.digits.iter() {
            out.push(char::from(d));
        }
        if self.exponent != 0 {
            out.push('e');
            out.push_str(&self.exponent.to_string());
        }
    }

    /// The magnitude's comparison with `other`'s, ignoring signs.
    fn cmp_magnitude(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        // The position just past the most significant digit decides first.
        let self_top = self.exponent.saturating_add(len_i64(&self.digits));
        let other_top = other.exponent.saturating_add(len_i64(&other.digits));
        self_top
            .cmp(&other_top)
            .then_with(|| self.digits.iter().cmp(other.digits.iter()))
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.cmp_magnitude(other),
            (true, true) => other.cmp_magnitude(self),
        }
    }
}

fn len_i64(digits: &[u8]) -> i64 {
    i64::try_from(digits.len()).unwrap_or(i64::MAX)
}

/// `10^exponent mod modulus` by squaring, in `O(log exponent)` steps.
fn pow10_mod(mut exponent: u64, modulus: u128) -> u128 {
    let mut result: u128 = 1 % modulus;
    let mut base: u128 = 10 % modulus;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = (result * base) % modulus;
        }
        base = (base * base) % modulus;
        exponent >>= 1;
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn d(text: &str) -> Decimal {
        Decimal::parse(text).unwrap()
    }

    fn canonical(text: &str) -> String {
        let mut out = String::new();
        d(text).write_canonical(&mut out);
        out
    }

    #[test]
    fn test_parse_equivalent_spellings_normalize_to_one_value() {
        assert_eq!(d("1"), d("1.0"));
        assert_eq!(d("1"), d("10e-1"));
        assert_eq!(d("100"), d("1e2"));
        assert_eq!(d("0"), d("-0"));
        assert_eq!(d("0"), d("0.000e7"));
        assert_eq!(d("-12.50"), d("-125e-1"));
        assert_eq!(canonical("120.0"), "12e1");
        assert_eq!(canonical("-0.0"), "0");
        assert_eq!(canonical("0.25"), "25e-2");
    }

    #[test]
    fn test_exponents_beyond_the_range_are_not_decimals() {
        assert!(Decimal::parse("1e4503599627370496").is_some());
        assert!(Decimal::parse("1e4503599627370497").is_none());
        assert!(Decimal::parse("1e-4503599627370497").is_none());
        assert!(Decimal::parse("0.0001e-4503599627370496").is_none());
        assert!(Decimal::parse("1e99999999999999999999999999").is_none());
        // Zero has no exponent to speak of.
        assert_eq!(Decimal::parse("0e99999999999999999999999999"), Some(d("0")));
        // Trailing zeros fold into the exponent before the check.
        assert!(Decimal::parse("10e4503599627370495").is_some());
    }

    #[test]
    fn test_parse_invalid_text_is_none() {
        for text in ["", "-", "1.", ".5", "1e", "1e+", "01x", "1 ", "+1"] {
            assert!(Decimal::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn test_cmp_orders_exactly_beyond_f64_precision() {
        assert!(d("9007199254740993") > d("9007199254740992"));
        assert!(d("-1") < d("0"));
        assert!(d("-2") < d("-1.5"));
        assert!(d("0.1") < d("0.10000000000000001"));
        assert!(d("1e400") > d("1e399"));
        assert!(d("-1e400") < d("-1e399"));
        assert_eq!(d("1.5").cmp(&d("15e-1")), Ordering::Equal);
    }

    #[test]
    fn test_is_integer_counts_whole_fractions() {
        assert!(d("1.0").is_integer());
        assert!(d("1e3").is_integer());
        assert!(d("0").is_integer());
        assert!(!d("1.5").is_integer());
        assert!(!d("1e-1").is_integer());
    }

    #[test]
    fn test_multiple_of_is_exact_for_decimal_fractions() {
        assert!(d("0.3").is_multiple_of(&d("0.1")));
        assert!(d("10").is_multiple_of(&d("2.5")));
        assert!(d("0").is_multiple_of(&d("7")));
        assert!(!d("0.35").is_multiple_of(&d("0.1")));
        assert!(!d("7").is_multiple_of(&d("2")));
        assert!(
            !d("1e300").is_multiple_of(&d("7e299")),
            "10 is not a multiple of 7"
        );
        assert!(d("1e30").is_multiple_of(&d("8")));
        assert!(!d("1e30").is_multiple_of(&d("3")));
        assert!(d("-6").is_multiple_of(&d("3")));
    }

    #[test]
    fn test_multiple_of_huge_exponent_stays_fast() {
        // 10^(2^52) mod 9 = 1; computed by squaring, not by looping.
        assert!(!d("1e4503599627370496").is_multiple_of(&d("9")));
        assert!(d("1e4503599627370496").is_multiple_of(&d("5")));
    }

    #[test]
    fn test_to_u64_saturating_bounds() {
        assert_eq!(d("5").to_u64_saturating(), Some(5));
        assert_eq!(d("5.0").to_u64_saturating(), Some(5));
        assert_eq!(d("1e30").to_u64_saturating(), Some(u64::MAX));
        assert_eq!(
            d("18446744073709551616").to_u64_saturating(),
            Some(u64::MAX)
        );
        assert_eq!(d("-1").to_u64_saturating(), None);
        assert_eq!(d("1.5").to_u64_saturating(), None);
    }

    #[test]
    fn test_divisor_digit_limit() {
        assert_eq!(d("1234567890123456789").mantissa_digits(), 19);
        assert!(!d("10").is_multiple_of(&d("1234567890123456789")));
        // A divisor too wide for u64 is never a divisor here; the compiler
        // refuses such a schema before it gets this far.
        assert!(!d("1").is_multiple_of(&d("12345678901234567890123")));
    }
}
