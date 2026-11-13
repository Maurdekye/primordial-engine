//! Field units (world-model.md 3.6): a small grammar with one canonical spelling per unit.
//!
//! A field's unit is part of its component's description, so it is in the registry's hashed bytes (6.6). Two
//! spellings of one unit would hash two ways, so a description only accepts the canonical spelling, and "no unit"
//! has exactly one form, the empty string.
//!
//! **Grammar.** A unit is a product of [`SYMBOLS`], each with an integer power:
//!
//! ```text
//! unit   := factor (op factor)*        op := "*" | "/"      (left to right: "J/kg/K" is J·kg⁻¹·K⁻¹)
//! factor := "1" | symbol ("^" "-"? digits)?
//! ```
//!
//! [`canonical_unit`] reads any spelling the grammar allows and writes the canonical one:
//! - each symbol appears once, its powers summed; symbols whose powers cancel go;
//! - symbols with a positive power come first, in [`SYMBOLS`] order, joined by `*`; then each symbol with a negative
//!   power, in [`SYMBOLS`] order, as `/symbol`; with no positive power the unit starts `1/`;
//! - a power of 1 is not written, any other as `^n` (2 to 99);
//! - no spaces, no SI prefixes (a field stores the SI unit's magnitude; a field in another scale has no unit).
//!
//! So `m/s`, `kg/m^3`, `1/s`, `N*m`, `J/kg/K`. The grammar fixes spelling, not dimension: `N` and `kg*m/s^2` are
//! both canonical and differ, and a field is labelled with the one it means. A dimensionless result (`1`, `m/m`)
//! is not a unit: such a field leaves its unit empty. Quantities outside the grammar (colours, frames, degrees)
//! leave it empty too. A symbol added to the end of [`SYMBOLS`] later makes new strings valid without changing any
//! existing one's spelling.

/// The unit symbols, in canonical order: the SI derived units with special names, then the base units, then the
/// two angle units.
pub const SYMBOLS: [&str; 16] =
    ["N", "Pa", "J", "W", "C", "V", "Hz", "kg", "m", "s", "A", "K", "mol", "cd", "rad", "sr"];

/// The largest power a symbol may carry in a canonical unit.
pub const MAX_POWER: i32 = 99;

/// The canonical spelling of `unit` (module docs), or why it isn't a unit. Returns the empty string for a
/// dimensionless product such as `1` or `m/m`, which a description does not accept as a unit.
pub fn canonical_unit(unit: &str) -> Result<String, String> {
    if unit.is_empty() {
        return Err("empty".into());
    }
    let mut powers = [0i32; SYMBOLS.len()];
    let mut sign = 1i32;
    for (i, part) in split_ops(unit).enumerate() {
        let (op, factor) = part;
        match (i, op) {
            (0, None) => {}
            (0, Some(_)) => return Err(format!("{unit:?} starts with an operator")),
            (_, Some('*')) => sign = 1,
            (_, Some('/')) => sign = -1,
            _ => unreachable!("split_ops yields an operator after the first factor"),
        }
        if factor == "1" {
            continue;
        }
        let (sym, pow) = match factor.split_once('^') {
            None => (factor, 1),
            Some((s, p)) => {
                let digits = p.strip_prefix('-').unwrap_or(p);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 2 {
                    return Err(format!("{factor:?}: a power is an integer from -{MAX_POWER} to {MAX_POWER}"));
                }
                let n: i32 = p.parse().map_err(|_| format!("{factor:?}: bad power"))?;
                if n == 0 {
                    return Err(format!("{factor:?}: a power of 0"));
                }
                (s, n)
            }
        };
        let Some(k) = SYMBOLS.iter().position(|s| *s == sym) else {
            return Err(format!("{sym:?} is not a unit symbol (allowed: {})", SYMBOLS.join(" ")));
        };
        powers[k] += sign * pow;
        if powers[k].abs() > MAX_POWER {
            return Err(format!("{sym:?}: power beyond {MAX_POWER}"));
        }
    }
    let write = |out: &mut String, k: usize, p: i32| {
        out.push_str(SYMBOLS[k]);
        if p != 1 {
            out.push('^');
            out.push_str(&p.to_string());
        }
    };
    let mut out = String::new();
    for (k, &p) in powers.iter().enumerate().filter(|(_, p)| **p > 0) {
        if !out.is_empty() {
            out.push('*');
        }
        write(&mut out, k, p);
    }
    let any_den = powers.iter().any(|p| *p < 0);
    if out.is_empty() && any_den {
        out.push('1');
    }
    for (k, &p) in powers.iter().enumerate().filter(|(_, p)| **p < 0) {
        out.push('/');
        write(&mut out, k, -p);
    }
    Ok(out)
}

/// Checks a description's unit: empty, or canonical and not dimensionless. The error says what to write instead.
pub(crate) fn check_unit(unit: &str) -> Result<(), String> {
    if unit.is_empty() {
        return Ok(());
    }
    match canonical_unit(unit) {
        Ok(c) if c == unit => Ok(()),
        Ok(c) if c.is_empty() => Err(format!("unit {unit:?} is dimensionless: leave the unit empty")),
        Ok(c) => Err(format!("unit {unit:?} is not canonical: write {c:?}")),
        Err(e) => Err(format!("unit {unit:?}: {e}")),
    }
}

/// Splits at `*` and `/`: the first factor with no operator, then each operator with the factor after it. An empty
/// factor (`m**s`, `m/`) comes out empty and fails as an unknown symbol.
fn split_ops(s: &str) -> impl Iterator<Item = (Option<char>, &str)> {
    let mut rest = s;
    let mut op = None;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let (factor, next) = match rest.find(['*', '/']) {
            Some(i) => (&rest[..i], Some((rest.as_bytes()[i] as char, &rest[i + 1..]))),
            None => (rest, None),
        };
        let item = (op, factor);
        match next {
            Some((o, r)) => {
                op = Some(o);
                rest = r;
            }
            None => done = true,
        }
        Some(item)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_forms() {
        for (input, canon) in [
            ("m", "m"),
            ("m/s", "m/s"),
            ("s^-1*m", "m/s"),
            ("kg/m^3", "kg/m^3"),
            ("m^-3*kg", "kg/m^3"),
            ("1/s", "1/s"),
            ("s^-1", "1/s"),
            ("m*N", "N*m"),
            ("J/K/kg", "J/kg/K"),
            ("J/kg*K", "J*K/kg"),
            ("m*m", "m^2"),
            ("m^2/m", "m"),
            ("rad/s", "rad/s"),
            ("kg*m/s^2", "kg*m/s^2"),
            ("1", ""),
            ("m/m", ""),
        ] {
            assert_eq!(canonical_unit(input).as_deref(), Ok(canon), "{input}");
            if !canon.is_empty() {
                assert_eq!(canonical_unit(canon).as_deref(), Ok(canon), "canonical is a fixed point: {canon}");
            }
        }
    }

    #[test]
    fn refused() {
        for bad in [
            "", "meter", "mm", "m3", "m ", " m", "m**s", "m/", "/s", "*m", "m^", "m^0", "m^x", "m^100", "m^-", "srgb",
            "deg", "m^1.5",
        ] {
            assert!(canonical_unit(bad).is_err(), "{bad:?} accepted");
        }
        assert!(canonical_unit("m*".repeat(99).trim_end_matches('*')).is_ok());
        assert!(canonical_unit("m*".repeat(100).trim_end_matches('*')).is_err());
    }

    #[test]
    fn check_accepts_only_canonical() {
        assert_eq!(check_unit(""), Ok(()));
        assert_eq!(check_unit("kg/m^3"), Ok(()));
        assert!(check_unit("m^-3*kg").unwrap_err().contains("\"kg/m^3\""));
        assert!(check_unit("1").unwrap_err().contains("dimensionless"));
        assert!(check_unit("m/m").unwrap_err().contains("dimensionless"));
        assert!(check_unit("meter").is_err());
    }

    #[test]
    fn symbols_are_distinct() {
        for (i, a) in SYMBOLS.iter().enumerate() {
            assert!(!a.is_empty() && a.bytes().all(|b| b.is_ascii_alphabetic()));
            assert!(SYMBOLS[i + 1..].iter().all(|b| b != a));
        }
    }
}
