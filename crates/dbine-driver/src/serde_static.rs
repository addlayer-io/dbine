//! Reading back `&'static str` fields: driver metadata comes from the
//! plugins' manifest (see `dbine-plugin`) instead of the driver's code.
//! Each distinct string is leaked once and reused afterwards (interned), so
//! memory stays bounded however often the same values are read.

use serde::{Deserialize, Deserializer};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn set() -> &'static Mutex<HashSet<&'static str>> {
    static SET: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// `s` with a `'static` lifetime: the same one for equal strings.
pub fn intern(s: &str) -> &'static str {
    let mut set = set().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = set.get(s) {
        return found;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// How many distinct strings were interned (tests).
pub fn interned() -> usize {
    set().lock().map(|s| s.len()).unwrap_or(0)
}

pub fn str<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    String::deserialize(d).map(|s| intern(&s))
}

pub fn strs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<&'static str>, D::Error> {
    Vec::<String>::deserialize(d).map(|v| v.iter().map(|s| intern(s)).collect())
}

pub fn pairs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(&'static str, &'static str)>, D::Error> {
    Vec::<(String, String)>::deserialize(d).map(|v| v.iter().map(|(a, b)| (intern(a), intern(b))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_are_leaked_once() {
        let a = intern("dbine-test-interned");
        let n = interned();
        let b = intern(&String::from("dbine-test-interned"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(interned(), n);
    }
}
