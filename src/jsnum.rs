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
use crate::json_numbers::normalize;

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
    let mut v: Value = serde_json::from_str(&normalize(s, false))?;
    js_order(&mut v);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_as_js_prints_them() {
        assert_eq!(normalize(r#"{"a":-0.0,"b":1.0,"c":0.1,"d":"-0.0","e":12345678901234567890,"f":1e-7,"g":-3}"#, true),
            r#"{"a":0,"b":1,"c":0.1,"d":"-0.0","e":12345678901234567000,"f":1e-7,"g":-3}"#);
        assert_eq!(normalize("[1.5e300,2.5E-3]", true), "[1.5e300,0.0025]");
    }

    #[test]
    fn fractional_fast_path_preserves_rounding_and_integer_accessors() {
        let cases = ["-0", "-0.0", "1.0", "1e3", "2.5E-3", "1e-7", "5e-324", "-5e-324",
            "9007199254740991", "9007199254740993", "12345678901234567890", "18446744073709551615",
            "-9223372036854775809", "1e21", "1.7976931348623157e308", "0.10000000000000001"];
        let compare = |s: &str| {
            let fast = parse(s).unwrap();
            let slow: Value = serde_json::from_str(&normalize(s, true)).unwrap();
            assert_eq!(fast.to_string(), slow.to_string(), "{s}");
            assert_eq!(fast.as_u64(), slow.as_u64(), "{s}");
            assert_eq!(fast.as_i64(), slow.as_i64(), "{s}");
            assert_eq!(fast.as_f64().map(f64::to_bits), slow.as_f64().map(f64::to_bits), "{s}");
        };
        for s in cases { compare(s); }
        // Deterministic float bit patterns cover subnormals, both signs,
        // exponent spelling, and large integers that cannot fit in a u64.
        let mut bits = 17u64;
        for _ in 0..10000 {
            bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
            let x = f64::from_bits(bits);
            if x.is_finite() { compare(&format!("{x:e}")); }
        }
        assert_eq!(parse(r#"{"s":"1.0\\\"-0","x":2.5E-3}"#).unwrap()["s"], Value::String("1.0\\\"-0".into()));
    }
    #[test]
    fn index_keys_first() {
        let v = parse(r#"{"b":1,"10":2,"2":3,"01":4}"#).unwrap();
        assert_eq!(v.to_string(), r#"{"2":3,"10":2,"b":1,"01":4}"#);
    }
}
