//! Primordial Engine core (design 23.1, package P2-1): the deterministic building blocks every sim crate
//! shares.
//!
//! * [`ids`]: 128-bit derived object ids, member keys, line, chain and entry ids, and the frame counter
//!   (world-model.md section 2).
//! * [`commit`]: BLAKE3, the 256-bit hash for every commitment (world-model.md 6.1).
//! * [`clock`]: the fixed-step tick rate, the frame clock and the slewed session clock (design 5, 10.4).
//! * [`dmath`]: bit-exact `f32` transcendental functions (design 4.3 rule 3).
//! * [`rng`]: counter-based RNG streams derived from the session seed (design 4.3 rule 2).
//! * [`hash`]: a pinned XXH64, for non-commitment uses only (physics test records, caches).
//! * [`coords`]: [`Pos`] and [`Offset`] behind the type wall, and hierarchical cells (world-model.md section 4).
//! * [`fpenv`]: the per-tick floating-point environment check (design 4.3 rule 9).
//! * [`Error`]: errors shared by the sim crates.
//!
//! Everything here uses only IEEE `+ - * /`, `sqrt`, comparisons, conversions and integer arithmetic,
//! so results are identical on every target that has IEEE `f32`/`f64` without contraction (Rust never
//! contracts to FMA). All byte encodings are little-endian.

#[cfg(target_endian = "big")]
compile_error!("pe-core assumes a little-endian target: sim state is hashed and copied as raw bytes");

pub mod clock;
pub mod commit;
pub mod coords;
pub mod dmath;
mod error;
pub mod fpenv;
pub mod hash;
pub mod ids;
pub mod rng;

pub use commit::Hash;
pub use coords::{Offset, OutOfRange, Pos};
pub use error::Error;
pub use ids::{ChainId, EntryRef, Frame, LineId, MemberKey, ObjectId};
