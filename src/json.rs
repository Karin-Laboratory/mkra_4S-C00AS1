//! A JSON writer sized to this job.
//!
//! `serde_json` would be the normal choice, but the whole server emits eight
//! object shapes and the target has 300 MB of RAM and no crates.io mirror. A
//! streaming writer with explicit scopes keeps the binary small and makes the
//! response shapes readable next to the Mirakurun schema they mirror.

use std::fmt::Write as _;

/// Append `s` as a JSON string literal, including the surrounding quotes.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // JSON requires escaping everything below 0x20; the rest goes out
            // as UTF-8, which is what every Mirakurun client expects.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `write_str` into a fresh `String`. Test-only today; kept next to
/// `write_str` because that is where its behaviour is defined.
#[cfg(test)]
pub fn to_string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    write_str(&mut o, s);
    o
}

/// An open JSON object. Commas are handled by the scope, so callers never
/// have to track whether a field is first.
pub struct Obj<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> Obj<'a> {
    pub fn new(out: &'a mut String) -> Self {
        out.push('{');
        Obj { out, first: true }
    }

    fn key(&mut self, k: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        write_str(self.out, k);
        self.out.push(':');
    }

    pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        write_str(self.out, v);
        self
    }

    pub fn num(&mut self, k: &str, v: impl Into<i64>) -> &mut Self {
        self.key(k);
        let _ = write!(self.out, "{}", v.into());
        self
    }

    pub fn u64(&mut self, k: &str, v: u64) -> &mut Self {
        self.key(k);
        let _ = write!(self.out, "{v}");
        self
    }

    pub fn f64(&mut self, k: &str, v: f64) -> &mut Self {
        self.key(k);
        // JSON has no NaN or Infinity; emit null rather than invalid output.
        if v.is_finite() {
            let _ = write!(self.out, "{v}");
        } else {
            self.out.push_str("null");
        }
        self
    }

    pub fn bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.key(k);
        self.out.push_str(if v { "true" } else { "false" });
        self
    }

    /// Insert already-encoded JSON. The caller owns its validity.
    pub fn raw(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        self.out.push_str(v);
        self
    }

    pub fn obj(&mut self, k: &str, f: impl FnOnce(&mut Obj)) -> &mut Self {
        self.key(k);
        let mut o = Obj::new(self.out);
        f(&mut o);
        o.end();
        self
    }

    pub fn arr(&mut self, k: &str, f: impl FnOnce(&mut Arr)) -> &mut Self {
        self.key(k);
        let mut a = Arr::new(self.out);
        f(&mut a);
        a.end();
        self
    }

    pub fn end(self) {
        self.out.push('}');
    }
}

/// An open JSON array.
pub struct Arr<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> Arr<'a> {
    pub fn new(out: &'a mut String) -> Self {
        out.push('[');
        Arr { out, first: true }
    }

    fn sep(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.sep();
        write_str(self.out, v);
        self
    }

    /// Kept for symmetry with `Obj::num`; every array of numbers in the
    /// current API surface happens to be empty (`gatheringNetworks`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn num(&mut self, v: impl Into<i64>) -> &mut Self {
        self.sep();
        let _ = write!(self.out, "{}", v.into());
        self
    }

    pub fn obj(&mut self, f: impl FnOnce(&mut Obj)) -> &mut Self {
        self.sep();
        let mut o = Obj::new(self.out);
        f(&mut o);
        o.end();
        self
    }

    pub fn end(self) {
        self.out.push(']');
    }
}

/// Build an object into a fresh `String`.
pub fn object(f: impl FnOnce(&mut Obj)) -> String {
    let mut s = String::new();
    let mut o = Obj::new(&mut s);
    f(&mut o);
    o.end();
    s
}

/// Build an array into a fresh `String`.
pub fn array(f: impl FnOnce(&mut Arr)) -> String {
    let mut s = String::new();
    let mut a = Arr::new(&mut s);
    f(&mut a);
    a.end();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_characters_and_quotes() {
        assert_eq!(to_string("a\"b"), r#""a\"b""#);
        assert_eq!(to_string("a\\b"), r#""a\\b""#);
        assert_eq!(to_string("a\nb"), r#""a\nb""#);
        // JSON forbids raw control characters; they must be \u-escaped.
        assert_eq!(to_string("\u{1}"), r#""\u0001""#);
        assert_eq!(to_string("a\tb"), r#""a\tb""#);
    }

    #[test]
    fn passes_utf8_through_unescaped() {
        // Service names are Japanese; escaping them would still be valid JSON
        // but bloats every channel listing.
        assert_eq!(to_string("NHK　BSP4K"), "\"NHK　BSP4K\"");
    }

    #[test]
    fn nested_scopes_place_commas_correctly() {
        let s = object(|o| {
            o.str("type", "BS")
                .num("n", 3)
                .bool("ok", true)
                .obj("inner", |i| {
                    i.num("a", 1).num("b", 2);
                })
                .arr("list", |a| {
                    a.num(1).num(2).obj(|o| {
                        o.str("k", "v");
                    });
                });
        });
        assert_eq!(
            s,
            r#"{"type":"BS","n":3,"ok":true,"inner":{"a":1,"b":2},"list":[1,2,{"k":"v"}]}"#
        );
    }

    #[test]
    fn empty_containers_are_valid() {
        assert_eq!(array(|_| {}), "[]");
        assert_eq!(object(|_| {}), "{}");
    }

    #[test]
    fn non_finite_numbers_become_null() {
        let s = object(|o| {
            o.f64("a", f64::NAN).f64("b", 1.5);
        });
        assert_eq!(s, r#"{"a":null,"b":1.5}"#);
    }
}
