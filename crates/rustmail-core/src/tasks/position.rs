//! String position keys: a reorder rewrites one row, never its neighbours.
//!
//! Keys are base-36 digit strings compared bytewise. `between` returns a key
//! strictly between two others and never ends in `0`, so a key always exists.

const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const BASE: u8 = 36;

fn digit(c: u8) -> u8 {
    ALPHABET.iter().position(|&a| a == c).unwrap_or(0) as u8
}

/// A key strictly after `a` and before `b` (`None` = open end; `a` empty =
/// open start). Requires `a < b`; if not, the result is simply after `a`.
pub fn between(a: &str, b: Option<&str>) -> String {
    let a = a.as_bytes();
    let b = b.map(str::as_bytes).filter(|b| *b > a);
    let mut out = Vec::new();
    let mut tight = b.is_some();
    let mut i = 0;
    loop {
        let lo = a.get(i).map_or(0, |&c| digit(c));
        let hi = match (tight, b) {
            (true, Some(b)) => b.get(i).map_or(0, |&c| digit(c)),
            _ => BASE,
        };
        if hi - lo > 1 {
            out.push(ALPHABET[((lo + hi) / 2) as usize]);
            return String::from_utf8(out).expect("ascii");
        }
        out.push(ALPHABET[lo as usize]);
        if hi > lo {
            tight = false;
        }
        i += 1;
    }
}

/// A key after every key in `last` (the current final key, if any).
pub fn after(last: Option<&str>) -> String {
    between(last.unwrap_or(""), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_ordered_when_inserting_everywhere() {
        let mut keys = vec![after(None)];
        for _ in 0..40 {
            keys.push(after(keys.last().map(String::as_str)));
        }
        // Insert repeatedly at the front and in the middle.
        for _ in 0..40 {
            let k = between("", Some(&keys[0]));
            assert!(k < keys[0]);
            keys.insert(0, k);
        }
        for _ in 0..40 {
            let at = keys.len() / 2;
            let k = between(&keys[at - 1], Some(&keys[at]));
            assert!(keys[at - 1] < k && k < keys[at]);
            keys.insert(at, k);
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert!(keys.iter().all(|k| !k.ends_with('0') && !k.is_empty()));
    }
}
