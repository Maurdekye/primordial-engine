//! Colours on the wire (api.md 4.4; @coordinator-opus's colour rulings m18c14b2d02ea and mdca20250c6a2). A field
//! pe-sim marks a colour (`FieldDesc::color`: linear light, sRGB primaries, straight alpha; 3 or 4 `F32`) is written
//! as `"#rrggbb"`, `"#rrggbbaa"`, `{"srgb": [...]}` in 0-1, or `{"linear": [...]}`, never a bare list, which would
//! land as linear without complaint. sRGB goes to linear through pe-sdk's one 256-entry table, so a script's hex and
//! the API's give the same bits on every machine. A read shows hex only when the stored bits are exactly what that
//! hex converts to, else `{"linear": [...]}`, so writing a read back never changes a colour.

use pe_sdk::color::{SRGB8_TO_LINEAR_BITS, alpha8, srgb8_to_linear};
use serde_json::{Value as Json, json};

/// What a colour field takes, for a refusal: `n` is its element count (3, or 4 with alpha).
pub fn expected(n: usize) -> &'static str {
    if n == 4 {
        "a colour: \"#rrggbb\" or \"#rrggbbaa\" (sRGB), {\"srgb\": [r, g, b, a]} in 0-1 (alpha optional), or \
         {\"linear\": [r, g, b, a]}"
    } else {
        "a colour: \"#rrggbb\" (sRGB), {\"srgb\": [r, g, b]} in 0-1, or {\"linear\": [r, g, b]}"
    }
}

/// How far outside 0-1 an `{"srgb"}` value may stray by rounding (ruling m680fdf640050).
const SRGB_SLACK: f64 = 1e-6;

/// An sRGB value in 0-1 as its 8-bit code: round(v × 255), clamped only within the rounding slack (rulings
/// mdca20250c6a2, m680fdf640050).
fn code(v: f64) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

/// `v` as the `n` stored linear values (`n` = 3 or 4), or why not.
pub fn encode(v: &Json, n: usize) -> Result<Vec<f32>, String> {
    let bad = |why: &str| Err(format!("{why}; give {}.", expected(n)));
    let opaque = |mut c: Vec<f32>| {
        if n == 4 && c.len() == 3 {
            c.push(1.0);
        }
        c
    };
    match v {
        Json::String(s) => {
            let digits = s.strip_prefix('#').filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
            let Some(d) = digits.filter(|d| d.len() == 6 || d.len() == 8) else {
                return bad(&format!("{s:?} isn't \"#rrggbb\" or \"#rrggbbaa\""));
            };
            if d.len() == 8 && n == 3 {
                return bad("this colour has no alpha, so it takes six hex digits, not eight");
            }
            let byte = |k: usize| u8::from_str_radix(&d[2 * k..2 * k + 2], 16).expect("checked hex");
            let mut c: Vec<f32> = (0..3).map(|k| srgb8_to_linear(byte(k))).collect();
            if d.len() == 8 {
                c.push(alpha8(byte(3)));
            }
            Ok(opaque(c))
        }
        Json::Object(o) if o.len() == 1 => {
            let (space, vals) = o.iter().next().expect("one entry");
            if space != "srgb" && space != "linear" {
                return bad(&format!("{space:?} isn't a colour space this API takes"));
            }
            let nums: Option<Vec<f64>> =
                vals.as_array().and_then(|a| a.iter().map(|x| x.as_f64().filter(|f| f.is_finite())).collect());
            let Some(nums) = nums else {
                return bad(&format!("{space:?} takes a list of finite numbers"));
            };
            // 0-255 values written as srgb would all clamp to 255, a silent white (ruling m680fdf640050): refused.
            if space == "srgb" && nums.iter().any(|&x| !(-SRGB_SLACK..=1.0 + SRGB_SLACK).contains(&x)) {
                return Err(format!(
                    "\"srgb\" takes values in 0-1; for 0-255 values use hex, such as \"#d22b2b\"; give {}.",
                    expected(n)
                ));
            }
            match space.as_str() {
                "srgb" if nums.len() == 3 || (n == 4 && nums.len() == 4) => {
                    let mut c: Vec<f32> = nums[..3].iter().map(|&x| srgb8_to_linear(code(x))).collect();
                    if let Some(&a) = nums.get(3) {
                        c.push(alpha8(code(a)));
                    }
                    Ok(opaque(c))
                }
                "linear" if nums.len() == n => {
                    let c: Vec<f32> = nums.iter().map(|&x| x as f32).collect();
                    if c.iter().any(|x| !x.is_finite()) {
                        return bad("a linear value is outside f32's range");
                    }
                    Ok(c)
                }
                _ => bad(&format!("{space:?} has the wrong number of values")),
            }
        }
        Json::Array(_) => bad("a bare list of numbers doesn't say its colour space"),
        _ => bad("that isn't a colour"),
    }
}

/// The 8-bit sRGB code whose linear value has exactly these bits, if any.
fn srgb_code(x: f32) -> Option<u8> {
    // The table rises with the code and its values are non-negative, so their bit patterns rise too.
    SRGB8_TO_LINEAR_BITS.binary_search(&x.to_bits()).ok().map(|k| k as u8)
}

/// The 8-bit alpha whose straight alpha has exactly these bits, if any.
fn alpha_code(x: f32) -> Option<u8> {
    let a = (f64::from(x) * 255.0).round();
    ((0.0..=255.0).contains(&a) && alpha8(a as u8).to_bits() == x.to_bits()).then_some(a as u8)
}

/// Stored linear values (3 or 4) as the API shows them: hex when exact, an opaque alpha left out; else linear, each
/// value in its shortest f32 form.
pub fn decode(c: &[f32]) -> Json {
    let rgb: Option<Vec<u8>> = c.iter().take(3).map(|&x| srgb_code(x)).collect();
    let alpha = match c.get(3) {
        None => Some(None),
        Some(&a) => alpha_code(a).map(Some),
    };
    if let (Some(rgb), Some(a)) = (rgb, alpha)
        && rgb.len() == 3
    {
        let mut s = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
        if let Some(a) = a.filter(|&a| a != 255) {
            s.push_str(&format!("{a:02x}"));
        }
        return json!(s);
    }
    let shortest: Vec<Json> = c.iter().map(|x| json!(x.to_string().parse::<f64>().unwrap_or(f64::from(*x)))).collect();
    json!({ "linear": shortest })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_srgb_and_linear_in_and_back() {
        // Hex through the table, alpha a/255, an opaque alpha left out on the way back.
        let red = encode(&json!("#d22b2b"), 4).unwrap();
        assert_eq!(red, vec![srgb8_to_linear(0xd2), srgb8_to_linear(0x2b), srgb8_to_linear(0x2b), 1.0]);
        assert_eq!(decode(&red), json!("#d22b2b"));
        let half = encode(&json!("#D22B2B80"), 4).unwrap();
        assert_eq!(half[3], 128.0 / 255.0);
        assert_eq!(decode(&half), json!("#d22b2b80"));
        assert_eq!(decode(&encode(&json!("#000000"), 3).unwrap()), json!("#000000"));
        // sRGB floats quantise to 8 bits: 0.5 is code 128 (127.5 rounds up), and it reads back as hex.
        let s = encode(&json!({"srgb": [1, 0.5, 0]}), 3).unwrap();
        assert_eq!(s, vec![1.0, srgb8_to_linear(128), 0.0]);
        assert_eq!(decode(&s), json!("#ff8000"));
        // Integers in srgb are 0-1 numbers like any other: [1, 0, 0] is red.
        assert_eq!(encode(&json!({"srgb": [1, 0, 0]}), 3).unwrap(), vec![1.0, 0.0, 0.0]);
        assert_eq!(encode(&json!({"srgb": [1.0000005, -0.0000005, 0.5]}), 3).unwrap()[..2], [1.0, 0.0]);
        // Linear passes exact, over 1 included, and reads back as linear when no hex gives those bits.
        let glow = encode(&json!({"linear": [4.0, 0.65, 0.0]}), 3).unwrap();
        assert_eq!(glow, vec![4.0, 0.65, 0.0]);
        assert_eq!(decode(&glow), json!({"linear": [4.0, 0.65, 0.0]}));
        assert_eq!(encode(&decode(&glow), 3).unwrap(), glow);
        // An alpha no 8-bit code gives reads back as linear too.
        assert_eq!(decode(&[1.0, 1.0, 1.0, 0.3]), json!({"linear": [1.0, 1.0, 1.0, 0.3]}));
        // Every 8-bit code reads back as itself.
        for k in 0..=255u8 {
            let c = encode(&json!(format!("#{k:02x}{k:02x}{k:02x}{k:02x}")), 4).unwrap();
            assert_eq!(encode(&decode(&c), 4).unwrap(), c, "code {k}");
        }
    }

    /// A `{linear}` read written back gives the same bits (@design-reviewer's Low on 0f6db6f3): the shortest f32
    /// decimal goes through f64 on both sides, so this checks that the two roundings never cross an f32 halfway point,
    /// over spread-out bit patterns of every exponent and the edges.
    #[test]
    fn a_linear_read_writes_back_to_the_same_bits() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut patterns: Vec<u32> = vec![0, 1, 0x007f_ffff, 0x0080_0000, 0x7f7f_ffff, 0x3f80_0000, 0x3f80_0001];
        patterns.extend(patterns.clone().iter().map(|b| b | 0x8000_0000));
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            patterns.push(x as u32);
        }
        for bits in patterns {
            let v = f32::from_bits(bits);
            if !v.is_finite() {
                continue;
            }
            // 4.0 in front keeps the read linear (no hex), whatever the value.
            let c = [4.0, v, 0.0];
            let back = encode(&decode(&c), 3).unwrap();
            assert_eq!(back[1].to_bits(), bits, "{v:e} ({bits:#010x}) came back as {:e}", back[1]);
        }
    }

    #[test]
    fn what_a_colour_refuses() {
        for (v, n, why) in [
            (json!([1, 0.5, 0]), 3, "bare list"),
            (json!("#d22b2b80"), 3, "six hex digits"),
            (json!("d22b2b"), 3, "isn't"),
            (json!("#d22b2"), 3, "isn't"),
            (json!({"srgb": [1, 0, 0, 1]}), 3, "wrong number"),
            (json!({"linear": [1, 0, 0]}), 4, "wrong number"),
            (json!({"hsv": [1, 0, 0]}), 3, "colour space"),
            (json!({"srgb": [1, 0, 0], "linear": [1, 0, 0]}), 3, "isn't a colour"),
            (json!({"linear": [1e39, 0, 0]}), 3, "f32"),
            (json!({"srgb": [1, "x", 0]}), 3, "finite numbers"),
            // 0-255 sRGB is refused, never clamped to white (ruling m680fdf640050).
            (json!({"srgb": [210, 43, 43]}), 3, "use hex"),
            (json!({"srgb": [1, 0, 0, 1.5]}), 4, "takes values in 0-1"),
            (json!({"srgb": [-0.1, 0, 0]}), 3, "takes values in 0-1"),
            (json!(0.5), 3, "isn't a colour"),
        ] {
            let e = encode(&v, n).unwrap_err();
            assert!(e.contains(why) && e.contains("give a colour"), "{v}: {e}");
        }
    }
}
