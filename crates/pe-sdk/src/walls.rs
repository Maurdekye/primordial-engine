//! The position wall's compile-fail tests (refound plan M2 acceptance: "code that takes plain numbers from a `Pos`
//! other than by `pos - ORIGIN`, saves a bare offset, or mixes offsets of two spaces does not compile"; logic.md 8).
//! Each refusal is a `compile_fail` example beside a twin that compiles, so a typo can't pass for a refusal. They run
//! under `cargo test -p pe-sdk --doc`. Each `compile_fail` example names the error it must fail with; stable rustdoc
//! ignores the code, nightly checks it (`cargo +nightly test -p pe-sdk --doc walls`, all codes checked 2026-11-27).
//!
//! **The one way to numbers** compiles:
//! ```
//! use pe_sdk::prelude::*;
//! let pos = ORIGIN + Offset::<World>::new(1.0, 2.0, 3.0);
//! let numbers: [f32; 3] = (pos - ORIGIN).to_array();
//! assert_eq!(numbers, [1.0, 2.0, 3.0]);
//! ```
//!
//! A `Pos`'s fields are private:
//! ```compile_fail,E0616
//! use pe_sdk::prelude::*;
//! let pos = ORIGIN + Offset::<World>::new(1.0, 2.0, 3.0);
//! let c = pos.chunk;
//! ```
//! ```compile_fail,E0616
//! use pe_sdk::prelude::*;
//! let pos = ORIGIN + Offset::<World>::new(1.0, 2.0, 3.0);
//! let l = pos.local;
//! ```
//!
//! No conversion to or from arrays:
//! ```compile_fail,E0277
//! use pe_sdk::prelude::*;
//! let pos = ORIGIN + Offset::<World>::new(1.0, 2.0, 3.0);
//! let a: [f64; 3] = pos.into();
//! ```
//! ```compile_fail,E0277
//! use pe_sdk::prelude::*;
//! let pos: Pos = [1.0f64, 2.0, 3.0].into();
//! ```
//!
//! No `Pos + Pos` and no scaling of a `Pos`; a `Pos` moves by an `Offset<World>`:
//! ```
//! use pe_sdk::prelude::*;
//! let a = ORIGIN + Offset::<World>::new(1.0, 0.0, 0.0);
//! let b = a + (a - ORIGIN) * 2.0;
//! assert_eq!((b - ORIGIN).to_array(), [3.0, 0.0, 0.0]);
//! ```
//! ```compile_fail,E0308
//! use pe_sdk::prelude::*;
//! let a = ORIGIN + Offset::<World>::new(1.0, 0.0, 0.0);
//! let b = a + a;
//! ```
//! ```compile_fail,E0369
//! use pe_sdk::prelude::*;
//! let a = ORIGIN + Offset::<World>::new(1.0, 0.0, 0.0);
//! let b = a * 2.0;
//! ```
//! A `Pos` moves only along world axes: an own-space offset needs the pose first.
//! ```compile_fail,E0308
//! use pe_sdk::prelude::*;
//! let a = ORIGIN + Offset::<Own>::new(1.0, 0.0, 0.0);
//! ```
//!
//! A `Transform`'s position fields are private; its place is a `Pos` for a root:
//! ```
//! use pe_sdk::engine::{Place, Transform};
//! fn root_pos(t: &Transform, pose: &pe_sdk::engine::Pose) -> Option<pe_sdk::space::Pos> {
//!     match t.place(pose) {
//!         Place::Root(p) => Some(p),
//!         Place::Child(_) => None,
//!     }
//! }
//! ```
//! ```compile_fail,E0616
//! fn numbers(t: &pe_sdk::engine::Transform) -> [f64; 3] {
//!     t.pos
//! }
//! ```
//! ```compile_fail,E0616
//! fn numbers(t: &pe_sdk::engine::Transform) -> [i64; 3] {
//!     t.chunk
//! }
//! ```
//! ```compile_fail,E0616
//! fn numbers(p: &pe_sdk::engine::Pose) -> pe_sdk::space::Pos {
//!     p.pos
//! }
//! ```
//!
//! Offsets of two spaces don't mix:
//! ```
//! use pe_sdk::prelude::*;
//! let a = Offset::<Own>::new(1.0, 0.0, 0.0);
//! let b = Offset::<Own>::new(0.0, 1.0, 0.0);
//! assert!((a + b).within(1.5));
//! ```
//! ```compile_fail,E0308
//! use pe_sdk::prelude::*;
//! let a = Offset::<Own>::new(1.0, 0.0, 0.0);
//! let b = Offset::<World>::new(0.0, 1.0, 0.0);
//! let c = a + b;
//! ```
//! ```compile_fail,E0308
//! use pe_sdk::prelude::*;
//! fn parent(v: Offset<Parent>) {}
//! parent(Offset::<Own>::new(1.0, 0.0, 0.0));
//! ```
//!
//! Offsets have no `==`:
//! ```compile_fail,E0369
//! use pe_sdk::prelude::*;
//! let same = Offset::<Own>::ZERO == Offset::<Own>::ZERO;
//! ```
//!
//! A saved offset names its reference: `Pos` and `OffsetFrom<Own | Parent>` are field types,
//! ```
//! pe_sdk::component! {
//!     pub struct Spot { home: pe_sdk::space::Pos, aim: pe_sdk::space::OffsetFrom<pe_sdk::space::Own> }
//! }
//! ```
//! a bare `Offset` is not,
//! ```compile_fail,E0277
//! pe_sdk::component! {
//!     pub struct Spot { aim: pe_sdk::space::Offset<pe_sdk::space::Own> }
//! }
//! ```
//! and neither is an offset saved along world axes, which has no reference:
//! ```compile_fail,E0277
//! pe_sdk::component! {
//!     pub struct Spot { aim: pe_sdk::space::OffsetFrom<pe_sdk::space::World> }
//! }
//! ```
//!
//! The SDK gives scripts no bytes or text of a `Pos` to parse (world-model.md 4.2, @chunks-reviewer's L6):
//! ```compile_fail,E0599
//! use pe_sdk::prelude::*;
//! let b = ORIGIN.encode();
//! ```
