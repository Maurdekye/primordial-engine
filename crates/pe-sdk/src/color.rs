//! Colours (world-model.md 3.6; logic.md 3.2; @coordinator-opus's ruling). A component field of type
//! [`LinearRgb`] or [`LinearRgba`] is marked a colour in the manifest: linear light, sRGB (Rec. 709) primaries,
//! straight alpha. Construction always names the space: [`LinearRgb::new`] takes linear values, [`LinearRgb::from_srgb8`]
//! and [`LinearRgb::from_hex`] take 8-bit sRGB codes through [`SRGB8_TO_LINEAR_BITS`]. There is no conversion from a
//! bare array into a colour, and no float sRGB constructor (it would need `powf`, whose bits differ between platforms).

/// An 8-bit sRGB code to linear light, as `f32` bit patterns: for `c = i / 255`, `c / 12.92` when `c <= 0.04045`,
/// else `((c + 0.055) / 1.055)^2.4`, computed in f64 and rounded to the nearest f32. Generated once, so no platform's
/// `powf` runs outside tests; pe-logic's `tests/colour.rs` checks every entry against the formula. This is the one
/// copy: pe-api's hex colours are to read it from here (@api-eng's part, not built yet), so a script's hex and the
/// WorldApi's give the same bits (the ruling's item 2). It is in pe-sdk because build identity B hashes pe-sdk's own sources, so the table can't change without B.
pub const SRGB8_TO_LINEAR_BITS: [u32; 256] = [
    0x00000000, 0x399f22b4, 0x3a1f22b4, 0x3a6eb40e, 0x3a9f22b4, 0x3ac6eb61, 0x3aeeb40e, 0x3b0b3e5d, 0x3b1f22b4,
    0x3b33070a, 0x3b46eb61, 0x3b5b518e, 0x3b70f18f, 0x3b83e1c6, 0x3b8fe616, 0x3b9c87fd, 0x3ba9c9b6, 0x3bb7ad6f,
    0x3bc6354a, 0x3bd56360, 0x3be539c1, 0x3bf5ba71, 0x3c0373b6, 0x3c0c6153, 0x3c15a705, 0x3c1f45be, 0x3c293e6b,
    0x3c3391f7, 0x3c3e4149, 0x3c494d44, 0x3c54b6c9, 0x3c607eb4, 0x3c6ca5df, 0x3c792d22, 0x3c830aa9, 0x3c89af9f,
    0x3c9085dc, 0x3c978dc6, 0x3c9ec7c2, 0x3ca63433, 0x3cadd37d, 0x3cb5a602, 0x3cbdac21, 0x3cc5e63a, 0x3cce54ac,
    0x3cd6f7d5, 0x3cdfd010, 0x3ce8ddba, 0x3cf2212d, 0x3cfb9ac3, 0x3d02a56a, 0x3d0798dd, 0x3d0ca7e6, 0x3d11d2af,
    0x3d171964, 0x3d1c7c30, 0x3d21fb3c, 0x3d2796b2, 0x3d2d4ebb, 0x3d332381, 0x3d39152b, 0x3d3f23e4, 0x3d454fd2,
    0x3d4b991d, 0x3d51ffec, 0x3d588468, 0x3d5f26b6, 0x3d65e6fd, 0x3d6cc563, 0x3d73c20e, 0x3d7add24, 0x3d810b65,
    0x3d84b793, 0x3d88732e, 0x3d8c3e48, 0x3d9018f4, 0x3d940344, 0x3d97fd49, 0x3d9c0715, 0x3da020ba, 0x3da44a4a,
    0x3da883d6, 0x3daccd6f, 0x3db12727, 0x3db5910f, 0x3dba0b38, 0x3dbe95b3, 0x3dc33090, 0x3dc7dbe0, 0x3dcc97b4,
    0x3dd1641d, 0x3dd6412b, 0x3ddb2eee, 0x3de02d76, 0x3de53cd4, 0x3dea5d18, 0x3def8e51, 0x3df4d090, 0x3dfa23e5,
    0x3dff885e, 0x3e027f06, 0x3e05427f, 0x3e080ea2, 0x3e0ae377, 0x3e0dc104, 0x3e10a753, 0x3e13966a, 0x3e168e51,
    0x3e198f0f, 0x3e1c98ac, 0x3e1fab30, 0x3e22c6a1, 0x3e25eb07, 0x3e29186a, 0x3e2c4ed0, 0x3e2f8e42, 0x3e32d6c5,
    0x3e362862, 0x3e39831f, 0x3e3ce703, 0x3e405417, 0x3e43ca60, 0x3e4749e6, 0x3e4ad2af, 0x3e4e64c3, 0x3e520029,
    0x3e55a4e7, 0x3e595305, 0x3e5d0a89, 0x3e60cb7a, 0x3e6495df, 0x3e6869be, 0x3e6c471f, 0x3e702e07, 0x3e741e7e,
    0x3e78188b, 0x3e7c1c33, 0x3e8014bf, 0x3e822039, 0x3e84308b, 0x3e8645b8, 0x3e885fc3, 0x3e8a7eb0, 0x3e8ca281,
    0x3e8ecb3b, 0x3e90f8df, 0x3e932b72, 0x3e9562f6, 0x3e979f6f, 0x3e99e0e0, 0x3e9c274c, 0x3e9e72b6, 0x3ea0c321,
    0x3ea31890, 0x3ea57307, 0x3ea7d288, 0x3eaa3716, 0x3eaca0b6, 0x3eaf0f68, 0x3eb18332, 0x3eb3fc15, 0x3eb67a14,
    0x3eb8fd34, 0x3ebb8576, 0x3ebe12de, 0x3ec0a56e, 0x3ec33d2a, 0x3ec5da14, 0x3ec87c30, 0x3ecb2380, 0x3ecdd008,
    0x3ed081ca, 0x3ed338c9, 0x3ed5f508, 0x3ed8b68a, 0x3edb7d52, 0x3ede4963, 0x3ee11abf, 0x3ee3f169, 0x3ee6cd65,
    0x3ee9aeb5, 0x3eec955b, 0x3eef815c, 0x3ef272b8, 0x3ef56974, 0x3ef86593, 0x3efb6716, 0x3efe6e00, 0x3f00bd2b,
    0x3f02460c, 0x3f03d1a5, 0x3f055ff7, 0x3f06f104, 0x3f0884cd, 0x3f0a1b54, 0x3f0bb499, 0x3f0d509f, 0x3f0eef65,
    0x3f1090ef, 0x3f12353d, 0x3f13dc50, 0x3f15862a, 0x3f1732cc, 0x3f18e237, 0x3f1a946e, 0x3f1c4970, 0x3f1e0140,
    0x3f1fbbde, 0x3f21794d, 0x3f23398c, 0x3f24fc9f, 0x3f26c285, 0x3f288b41, 0x3f2a56d2, 0x3f2c253c, 0x3f2df67f,
    0x3f2fca9c, 0x3f31a194, 0x3f337b6a, 0x3f35581d, 0x3f3737b0, 0x3f391a24, 0x3f3aff7a, 0x3f3ce7b2, 0x3f3ed2cf,
    0x3f40c0d2, 0x3f42b1bc, 0x3f44a58e, 0x3f469c49, 0x3f4895ef, 0x3f4a9280, 0x3f4c91ff, 0x3f4e946c, 0x3f5099c9,
    0x3f52a216, 0x3f54ad56, 0x3f56bb88, 0x3f58ccaf, 0x3f5ae0cc, 0x3f5cf7df, 0x3f5f11ea, 0x3f612eef, 0x3f634eee,
    0x3f6571e9, 0x3f6797e0, 0x3f69c0d5, 0x3f6becca, 0x3f6e1bbf, 0x3f704db5, 0x3f7282ae, 0x3f74baab, 0x3f76f5ae,
    0x3f7933b6, 0x3f7b74c6, 0x3f7db8de, 0x3f800000,
];

/// An 8-bit sRGB code in linear light ([`SRGB8_TO_LINEAR_BITS`]).
pub const fn srgb8_to_linear(code: u8) -> f32 {
    f32::from_bits(SRGB8_TO_LINEAR_BITS[code as usize])
}

/// An 8-bit alpha as a straight, linear alpha: `a / 255`, no curve.
pub const fn alpha8(a: u8) -> f32 {
    a as f32 / 255.0
}

/// A colour in linear light with sRGB primaries: a field of this type is marked a colour in the manifest, so the
/// WorldApi reads and writes it as one (hex or an explicit space, never a bare array). `[1.0, 1.0, 1.0]` is white.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct LinearRgb(pub [f32; 3]);

/// [`LinearRgb`] with straight (not premultiplied) alpha in the fourth element.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct LinearRgba(pub [f32; 4]);

impl LinearRgb {
    /// Linear values, as they are stored.
    pub const fn new(linear: [f32; 3]) -> LinearRgb {
        LinearRgb(linear)
    }

    /// 8-bit sRGB codes, as a colour picker or a CSS colour gives them.
    pub const fn from_srgb8(r: u8, g: u8, b: u8) -> LinearRgb {
        LinearRgb([srgb8_to_linear(r), srgb8_to_linear(g), srgb8_to_linear(b)])
    }

    /// `0xRRGGBB` in sRGB; anything above 24 bits panics (an `0xRRGGBBAA` is [`LinearRgba::from_hex`]'s).
    pub const fn from_hex(rgb: u32) -> LinearRgb {
        assert!(rgb <= 0xff_ffff, "LinearRgb::from_hex takes 0xRRGGBB");
        LinearRgb::from_srgb8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }
}

impl LinearRgba {
    /// Linear values and a straight alpha, as they are stored.
    pub const fn new(linear: [f32; 4]) -> LinearRgba {
        LinearRgba(linear)
    }

    /// 8-bit sRGB codes, and an 8-bit alpha taken as `a / 255` with no curve.
    pub const fn from_srgba8(r: u8, g: u8, b: u8, a: u8) -> LinearRgba {
        LinearRgba([srgb8_to_linear(r), srgb8_to_linear(g), srgb8_to_linear(b), alpha8(a)])
    }

    /// `0xRRGGBBAA`: sRGB, and the alpha as [`LinearRgba::from_srgba8`] takes it. Always eight digits: a six-digit
    /// `0xff8000` reads as `0x00ff8000`, a fully transparent green; for an opaque colour write `0xff8000ff`.
    pub const fn from_hex(rgba: u32) -> LinearRgba {
        LinearRgba::from_srgba8((rgba >> 24) as u8, (rgba >> 16) as u8, (rgba >> 8) as u8, rgba as u8)
    }
}

macro_rules! as_array {
    ($($t:ident: $n:literal),*) => {
        $(
            /// The stored values; there is no way back without naming the space.
            impl From<$t> for [f32; $n] {
                fn from(c: $t) -> [f32; $n] {
                    c.0
                }
            }
            impl core::ops::Index<usize> for $t {
                type Output = f32;
                fn index(&self, i: usize) -> &f32 {
                    &self.0[i]
                }
            }
            impl core::ops::IndexMut<usize> for $t {
                fn index_mut(&mut self, i: usize) -> &mut f32 {
                    &mut self.0[i]
                }
            }
        )*
    };
}

as_array!(LinearRgb: 3, LinearRgba: 4);

// Adding and scaling light is meaningful in linear light. Only for `LinearRgb`: with a straight alpha, whether a scale
// applies to the alpha has no one answer, so `LinearRgba` leaves it to the script.
impl core::ops::Add for LinearRgb {
    type Output = LinearRgb;
    fn add(self, o: LinearRgb) -> LinearRgb {
        LinearRgb([self.0[0] + o.0[0], self.0[1] + o.0[1], self.0[2] + o.0[2]])
    }
}

impl core::ops::Mul<f32> for LinearRgb {
    type Output = LinearRgb;
    fn mul(self, k: f32) -> LinearRgb {
        LinearRgb([self.0[0] * k, self.0[1] * k, self.0[2] * k])
    }
}
