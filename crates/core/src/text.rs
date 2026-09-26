//! String and number handling with JavaScript semantics.
//!
//! Recorded pages were rendered by browsers, and recorders measure and serialize text in the
//! browser's terms, so "length", "whitespace" and "a number as text" mean what they mean there:
//!
//! - lengths and offsets count UTF-16 code units, not bytes or chars;
//! - whitespace is ECMAScript `\s` (includes U+FEFF, excludes U+0085), not Unicode `White_Space`;
//! - numbers print like `String(n)` (`200`, not `200.0`) and round like `Math.round`/`toFixed`.

use serde_json::Value;

/// ECMAScript `WhiteSpace` and `LineTerminator`: what `\s` and `String.prototype.trim` match.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// Regex character class equivalent to ECMAScript `\s`.
pub const JS_SPACE_CLASS: &str = r"[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";

/// JS `s.trim()`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// JS `s.replace(/\s+/g, " ")`: whitespace runs become one space; ends are kept.
pub fn collapse_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// JS `s.replace(/\s+/g, " ").trim()`.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split(is_js_space).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// JS `s.replace(/\s+/g, "")`.
pub fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !is_js_space(*c)).collect()
}

/// JS `s.length`: UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.encode_utf16().count()
    }
}

/// JS `s.slice(start, end)` for non-negative offsets, in UTF-16 code units.
///
/// A cut through a surrogate pair yields U+FFFD where JavaScript would keep a lone surrogate.
pub fn utf16_slice(s: &str, start: usize, end: Option<usize>) -> String {
    let end = end.unwrap_or(usize::MAX);
    if s.is_ascii() {
        let end = end.min(s.len());
        return s.get(start.min(end)..end).unwrap_or_default().to_owned();
    }
    let units: Vec<u16> = s
        .encode_utf16()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Cut `s` to `max` UTF-16 units, marking the cut with `…` (which takes the last unit).
pub fn truncate(s: String, max: usize) -> String {
    if utf16_len(&s) > max {
        format!("{}…", utf16_slice(&s, 0, Some(max.saturating_sub(1))))
    } else {
        s
    }
}

/// Collapse whitespace, then [`truncate`].
pub fn clip(s: &str, max: usize) -> String {
    truncate(collapse_whitespace(s), max)
}

/// JS `Math.round`: halves round toward positive infinity.
pub fn round_half_up(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// JS `String(n)` for a number.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if n == 0.0 {
        return "0".into(); // including -0
    }
    let magnitude = n.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        // Rust's `{:e}` gives the same shortest digits; JS writes a sign on positive exponents.
        let formatted = format!("{n:e}");
        return match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => formatted,
        };
    }
    n.to_string()
}

/// JS `Number(s)` for the attribute values the compiler reads (`tabindex`, `data-col`).
pub fn parse_number(s: &str) -> f64 {
    let s = trim(s);
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |v| v as f64);
        }
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust also accepts "inf", "nan" and "infinity"; JavaScript does not.
    let lower = s.to_ascii_lowercase();
    if lower.contains("inf") || lower.contains("nan") {
        return f64::NAN;
    }
    s.parse().unwrap_or(f64::NAN)
}

/// JS `n.toFixed(1)`.
///
/// JavaScript rounds the exact binary value half away from zero, whereas Rust's formatter rounds
/// ties to even. This works from the significand so that scaling a near-tie by ten cannot
/// manufacture a tie that the exact value does not have.
pub fn to_fixed_1(value: f64) -> String {
    let magnitude = value.abs();
    if !magnitude.is_finite() || magnitude >= 1e21 {
        return number_to_string(value);
    }
    let bits = magnitude.to_bits();
    let biased_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = u128::from(bits & ((1_u64 << 52) - 1));
    let (significand, exponent) = if biased_exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1_u128 << 52), biased_exponent - 1075)
    };
    let tenths = significand * 10;
    let rounded = if exponent >= 0 {
        tenths << exponent
    } else if -exponent >= 128 {
        0
    } else {
        let divisor = 1_u128 << -exponent;
        tenths / divisor + u128::from(tenths % divisor >= divisor / 2)
    };
    // `(-0.001).toFixed(1)` is "-0.0" but `(-0).toFixed(1)` is "0.0": the sign follows `x < 0`.
    let sign = if value < 0.0 { "-" } else { "" };
    format!("{sign}{}.{}", rounded / 10, rounded % 10)
}

/// JS `String(value)` for a JSON value.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        // Array.prototype.toString joins elements with "," and renders null as "".
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_matches_javascript_on_ties_and_near_ties() {
        for (value, expected) in [
            (64.25, "64.3"),
            (1.15, "1.1"), // 1.15 is stored just below the tie
            (1.25, "1.3"),
            (-1.25, "-1.3"),
            (-0.001, "-0.0"),
            (0.0, "0.0"),
        ] {
            assert_eq!(to_fixed_1(value), expected, "{value}");
        }
    }

    #[test]
    fn numbers_print_like_javascript() {
        for (value, expected) in [
            (200.0, "200"),
            (1.5, "1.5"),
            (-0.0, "0"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (f64::NAN, "NaN"),
        ] {
            assert_eq!(number_to_string(value), expected);
        }
    }

    #[test]
    fn numeric_attributes_parse_like_javascript() {
        assert_eq!(parse_number(" 3 "), 3.0);
        assert_eq!(parse_number(""), 0.0);
        assert_eq!(parse_number("0x10"), 16.0);
        assert!(parse_number("inf").is_nan());
        assert!(parse_number("-1").is_sign_negative());
    }

    #[test]
    fn whitespace_follows_ecmascript() {
        assert_eq!(collapse_whitespace("\u{feff} a \u{a0} b "), "a b");
        assert_eq!(collapse_whitespace("a\u{85}b"), "a\u{85}b");
        assert_eq!(collapse_runs("  a  b "), " a b ");
    }
}
