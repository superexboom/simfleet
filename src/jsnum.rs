// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON AS A BROWSER CARRIES IT.
//!
//! In the page every engine answer goes through `JSON.parse` and back through
//! `JSON.stringify` before the next call reads it: numbers become f64 (`-0`
//! prints as `0`, an integer past 2^53 rounds), and an object's integer-like
//! keys move to the front in numeric order. The server compares a result's
//! bits with a browser's, so what this client hands the engine is what a
//! browser would have handed it — `normalize` for the numbers, `js_order` for
//! the keys.

use serde_json::{Map, Value};
use std::fmt::Write;

/// The text with every number re-spelt the way `JSON.stringify` would spell the
/// f64 it parses to. Strings are copied untouched.
pub fn normalize(s: &str) -> String {
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
                    out.push_str(&s[start..j0]);
                    fmt_js(x, &mut out);
                    start = j;
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

/// An array index as JS defines one: canonical digits below 2^32 − 1.
fn index_key(k: &str) -> Option<u32> {
    if k.is_empty() || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    k.parse::<u64>().ok().filter(|&n| n < u32::MAX as u64).map(|n| n as u32)
}

/// Every object's keys in the order a JS engine enumerates them.
pub fn js_order(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(js_order),
        Value::Object(m) => {
            m.values_mut().for_each(js_order);
            if m.keys().any(|k| index_key(k).is_some()) {
                let old = std::mem::take(m);
                let mut idx: Vec<(u32, String, Value)> = Vec::new();
                let mut rest: Vec<(String, Value)> = Vec::new();
                for (k, x) in old {
                    match index_key(&k) {
                        Some(n) => idx.push((n, k, x)),
                        None => rest.push((k, x)),
                    }
                }
                idx.sort_by_key(|e| e.0);
                let mut nm = Map::new();
                for (_, k, x) in idx {
                    nm.insert(k, x);
                }
                for (k, x) in rest {
                    nm.insert(k, x);
                }
                *m = nm;
            }
        }
        _ => {}
    }
}

/// Parse text the way a browser would hold it.
pub fn parse(s: &str) -> anyhow::Result<Value> {
    let mut v: Value = serde_json::from_str(&normalize(s))?;
    js_order(&mut v);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_as_js_prints_them() {
        assert_eq!(normalize(r#"{"a":-0.0,"b":1.0,"c":0.1,"d":"-0.0","e":12345678901234567890,"f":1e-7,"g":-3}"#),
            r#"{"a":0,"b":1,"c":0.1,"d":"-0.0","e":12345678901234567000,"f":1e-7,"g":-3}"#);
        assert_eq!(normalize("[1.5e300,2.5E-3]"), "[1.5e300,0.0025]");
    }
    #[test]
    fn index_keys_first() {
        let v = parse(r#"{"b":1,"10":2,"2":3,"01":4}"#).unwrap();
        assert_eq!(v.to_string(), r#"{"2":3,"10":2,"b":1,"01":4}"#);
    }
}
