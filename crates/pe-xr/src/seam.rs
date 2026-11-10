//! Seam S5's XR types, from pe-seams v0 (`pe_seams::view` and `pe_seams::time`), as `docs/spec/xr.md` section 8.1
//! proposed them. Re-exported here so the rest of pe-xr names them in one place.

pub use pe_seams::time::HostTime;
pub use pe_seams::view::{Hand, HandFeed, HandInput, HandSource, RigPose};
