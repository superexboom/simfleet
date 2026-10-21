// SPDX-License-Identifier: AGPL-3.0-or-later
use std::fmt::Write;

/// JS number spelling for integers and -0. `all` also formats fractions,
/// retained as the reference path in tests. Strings are copied untouched.
pub fn normalize(s: &str, all: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut start = 0; // start of the run copied verbatim
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() {
                match b[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if c == b'-' || c.is_ascii_digit() {
            let j0 = i;
            let mut j = i + 1;
            let mut plain = true;
            while j < b.len() && matches!(b[j], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                if !b[j].is_ascii_digit() {
                    plain = false;
                }
                j += 1;
            }
            let tok = &s[j0..j];
            // A short plain integer is already what JS would print.
            let digits = tok.trim_start_matches('-').len();
            if !(plain && digits <= 15 && tok != "-0") {
                if let Ok(x) = tok.parse::<f64>() {
                    // A non-integral finite f64 remains that same f64 under
                    // serde_json's float_roundtrip parser. Formatting it first
                    // only allocates and converts the same digits twice.
                    // Integers still need JS rounding and the integer spelling
                    // the engine's as_u64/as_i64 readers expect; -0 needs 0.
                    if all || !x.is_finite() || x.fract() == 0.0 {
                        out.push_str(&s[start..j0]);
                        fmt_js(x, &mut out);
                        start = j;
                    }
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out.push_str(&s[start..]);
    out
}

fn fmt_js(x: f64, out: &mut String) {
    let a = x.abs();
    if x == 0.0 {
        out.push('0');
    } else if x.fract() == 0.0 && a < 9007199254740992.0 {
        let _ = write!(out, "{x:.0}");
    } else if x.fract() == 0.0 && a < 1e21 {
        // JS prints the SHORTEST digits and pads with zeros (2^64 is
        // "18446744073709552000"); an integer read from it is that integer.
        let e = format!("{a:e}");
        let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
        let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
        let exp: usize = exp.parse().unwrap_or(0);
        if x < 0.0 {
            out.push('-');
        }
        out.push_str(&digits);
        for _ in digits.len()..=exp {
            out.push('0');
        }
    } else if !(1e-6..1e21).contains(&a) {
        let _ = write!(out, "{x:e}");
    } else {
        let _ = write!(out, "{x}");
    }
}
