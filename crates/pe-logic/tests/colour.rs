//! A script's colour fields (world-model.md 3.6; @coordinator-opus's colour ruling, item 1): a field of pe-sdk's
//! `LinearRgb` or `LinearRgba` type is marked a colour in the manifest, the mark reaches the store's description, and
//! so it is in the component's id. The engine's `Light.color` is marked, and pe-sdk's copy says so.

use pe_core::Hash;
use pe_logic::manifest;
use pe_sdk::component::{Component, FieldDesc, LinearRgb, LinearRgba, info};

// pe-sim's kind codes are pe-sdk's for the kinds both have (F32 is 9 in both); the colour test below relies on it.
use pe_sdk::layout::{ColorSpace, FieldKind, field_byte, split_field_byte};
use pe_sim::ComponentId;

pe_sdk::component! {
    /// A glow with a colour, a tint with alpha, and a plain triple.
    pub struct Glow { color: LinearRgb, tint: LinearRgba, plain: [f32; 3], intensity: f32 }
}

pe_sdk::component! {
    /// The same layout with nothing marked.
    pub struct Glow2 { color: [f32; 3], tint: [f32; 4], plain: [f32; 3], intensity: f32 }
}

const BATCH: (u32, u32) = (1 << 16, 1 << 12);

fn decoded(components: &[pe_sdk::component::ComponentInfo]) -> Result<manifest::Manifest, manifest::ManifestError> {
    let mut buf = vec![0u8; 4096];
    let n = pe_sdk::manifest::encode(&mut buf, 1, BATCH, components, &[]).expect("it fits");
    manifest::decode(&buf[..n])
}

#[test]
fn a_colour_field_is_marked_in_the_manifest_and_the_description() {
    let m = decoded(&[info::<Glow>()]).expect("the manifest");
    let colors: Vec<_> = m.components[0].fields.iter().map(|f| (f.name.as_str(), f.color)).collect();
    let linear = Some(ColorSpace::LinearSrgb);
    assert_eq!(colors, [("color", linear), ("tint", linear), ("plain", None), ("intensity", None)]);

    let d = pe_logic::run::component_desc(&m.components[0], ComponentId(1 << 40)).expect("a description");
    let sim: Vec<_> = d.fields().iter().map(|f| f.color).collect();
    let l = Some(pe_sim::ColorSpace::LinearSrgb);
    assert_eq!(sim, [l, l, None, None]);

    // The mark is in the id: the same names and layout unmarked are another component (world-model.md 7.9).
    let plain = decoded(&[info::<Glow2>()]).expect("the manifest");
    let plain = pe_logic::run::component_desc(&plain.components[0], ComponentId(1 << 40)).expect("a description");
    let v = Hash([7; 32]);
    let id = |d: &pe_sim::ComponentDesc| pe_world::script_component_id(&v, "Glow", d.fields().to_vec());
    assert_ne!(id(&d), id(&plain));
    assert!(id(&d).is_some() && id(&plain).is_some());
}

#[test]
fn a_colour_anywhere_but_three_or_four_f32_is_refused_with_why() {
    for (kind, count) in [(FieldKind::F32, 2), (FieldKind::F32, 5), (FieldKind::F64, 3), (FieldKind::U32, 4)] {
        let fields: &'static [FieldDesc] =
            Box::leak(Box::new([FieldDesc { name: "c", kind, count, offset: 0, color: Some(ColorSpace::LinearSrgb) }]));
        let size = kind.size() * count;
        let c = pe_sdk::component::ComponentInfo { name: "Bad", size, align: kind.size().min(8), fields };
        let e = decoded(&[c]).expect_err("refused");
        assert!(e.0.contains("a colour is 3 or 4 f32 elements"), "{kind:?} x {count}: {}", e.0);
    }
}

#[test]
fn the_field_byte_is_the_kind_with_the_space_in_its_high_bits() {
    // Unmarked, the byte is the kind's code, as before the mark, so no unmarked manifest changes.
    for code in 1..=13 {
        let kind = FieldKind::from_code(code).unwrap();
        assert_eq!(field_byte(kind, None), code);
        assert_eq!(split_field_byte(code), Some((kind, None)));
        let marked = field_byte(kind, Some(ColorSpace::LinearSrgb));
        assert_eq!(marked, code | 1 << 5);
        assert_eq!(split_field_byte(marked), Some((kind, Some(ColorSpace::LinearSrgb))));
    }
    // pe-sim's byte for the same field is the same byte (world-model.md 6.6).
    let f = pe_sim::FieldDesc::new("c", pe_sim::FieldKind::F32, 0, 3).with_color(pe_sim::ColorSpace::LinearSrgb);
    assert_eq!(f.kind_byte(FieldKind::F32 as u8), field_byte(FieldKind::F32, Some(ColorSpace::LinearSrgb)));
    // An unknown space, or an unknown kind, is refused.
    assert_eq!(split_field_byte(9 | 2 << 5), None);
    assert_eq!(split_field_byte(14), None);
}

#[test]
fn a_colour_reads_and_writes_like_its_array_and_is_built_naming_its_space() {
    let mut c = LinearRgb::new([1.0, 0.8, 0.6]);
    c[1] = 0.5;
    assert_eq!((c[0], c[1]), (1.0, 0.5));
    assert_eq!(<[f32; 3]>::from(c), [1.0, 0.5, 0.6]);
    assert_eq!(c + LinearRgb::new([0.0, 0.5, 0.4]), LinearRgb::new([1.0, 1.0, 1.0]));
    assert_eq!(c * 0.5, LinearRgb::new([0.5, 0.25, 0.3]));
    assert_eq!((size_of::<LinearRgb>(), align_of::<LinearRgba>()), (12, 4));
    // sRGB codes through the table: black, white, and mid-grey's linear value.
    assert_eq!(LinearRgb::from_hex(0x000000), LinearRgb::new([0.0; 3]));
    assert_eq!(LinearRgb::from_hex(0xffffff), LinearRgb::new([1.0; 3]));
    let grey = LinearRgb::from_srgb8(0x80, 0x80, 0x80);
    assert!((grey[0] - 0.215_860_5).abs() < 1e-6, "{grey:?}");
    let ff8000 = LinearRgb::from_hex(0xff8000);
    assert_eq!(ff8000, LinearRgb::from_srgb8(255, 128, 0));
    // The alpha is linear: a / 255, no curve.
    let half = LinearRgba::from_hex(0xff800080);
    assert_eq!(half, LinearRgba::from_srgba8(255, 128, 0, 128));
    assert_eq!(half[3], 128.0 / 255.0);
    assert_eq!(LinearRgba::from_hex(0x000000ff)[3], 1.0);
    assert!(std::panic::catch_unwind(|| LinearRgb::from_hex(0x1_000000)).is_err(), "8 hex digits on a LinearRgb");
}

/// Every entry of the table is the formula, computed in f64 and rounded to f32 (the table's own doc).
#[test]
fn the_srgb_table_is_the_formula() {
    for (i, &bits) in pe_sdk::color::SRGB8_TO_LINEAR_BITS.iter().enumerate() {
        let c = i as f64 / 255.0;
        let linear = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        assert_eq!(f32::from_bits(bits), linear as f32, "entry {i}");
        assert_eq!(pe_sdk::color::srgb8_to_linear(i as u8).to_bits(), bits);
    }
    assert!(pe_sdk::color::SRGB8_TO_LINEAR_BITS.windows(2).all(|w| f32::from_bits(w[0]) < f32::from_bits(w[1])));
}

#[test]
fn pe_sdk_s_light_marks_its_colour_as_the_engine_does() {
    let f = pe_sdk::engine::Light::FIELDS.iter().find(|f| f.name == "color").expect("Light.color");
    assert_eq!((f.kind, f.count, f.color), (FieldKind::F32, 3, Some(ColorSpace::LinearSrgb)));
    let engine = <pe_world::components::Light as pe_sim::Component>::desc();
    let e = engine.fields().iter().find(|f| f.name == "color").expect("the engine's Light.color");
    assert_eq!(e.color, Some(pe_sim::ColorSpace::LinearSrgb));
}
