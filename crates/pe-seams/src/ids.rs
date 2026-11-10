//! The id, hash and position types every seam uses: pe-core's own (world-model.md 9.1), re-exported.
//!
//! **Transition done.** pe-seams v0 carried copies of these shapes until WS-world's pe-core change landed
//! (world/core-v1); this module now re-exports pe-core's definitions, so there is one type of each. The golden
//! vectors below were produced by pe-core at 426d8420 and still pass, so nothing drifted.
//!
//! [`Pos`] is the type wall (the user's m06d2643578d7): no reader, no constructor from numbers, `Pos - Pos` total,
//! moving by [`Pos::checked_add`]. Its encoding, cells and rows are pe-core's.

pub use pe_core::Frame;
pub use pe_core::commit::{Hash, Hasher};
pub use pe_core::coords::{Offset, OutOfRange, Pos};
pub use pe_core::ids::{ChainId, EntryRef, LineId, MemberKey, ORIGIN_ENTRY, ObjectId};

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::coords::CHUNK;

    #[test]
    fn origin_line_is_plain_blake3_of_the_origin_entry() {
        assert_eq!(LineId::origin().0, Hash(*blake3::hash(b"pe.origin.v1").as_bytes()));
        // world-model's pinned value (9a570a9c...db9e), so a change to the origin bytes fails here too.
        let h = LineId::origin().0.0;
        assert_eq!((h[0], h[1], h[2], h[3]), (0x9a, 0x57, 0x0a, 0x9c));
        assert_eq!((h[30], h[31]), (0xdb, 0x9e));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Golden vectors, pinned from pe-core at 426d8420; any change to these derivations or encodings fails here.
    #[test]
    fn golden_vectors_match_pe_core_426d8420() {
        let line = LineId::origin();
        let key = MemberKey([0x11; 32]);
        let chain = ChainId::new(line, key, [0x22; 32]);
        assert_eq!(hex(&line.0.0), "9a570a9c46078da1b28a58daddf02dcea9c8bb31f4efc8ab61089b552b1ddb9e");
        assert_eq!(hex(&chain.0.0), "f9b15cc973e80406d2712f240a1d76008f015c16ca0f86b5f4bbde52c6869ac9");
        assert_eq!(
            format!("{:032x}", ObjectId::created(line, key, chain, 7, 3).to_u128()),
            "7b1e6a231cb93972bbf8ecaab2238c94"
        );
        assert_eq!(
            format!("{:032x}", ObjectId::spawned(line, ObjectId::from_u128(0x1234), Frame(99), 2).to_u128()),
            "6c8e7703bd9c45f9428b175321a10c71"
        );
        assert_eq!(
            format!("{:032x}", ObjectId::hand(line, key, 1, Frame(5)).to_u128()),
            "95b6a39b914e2646d926443016d398bd"
        );
        let p = Pos::ORIGIN.checked_add(Offset([-1500.25, 0.5, 3_000_000.0])).unwrap();
        let mut out = Vec::new();
        p.encode(&mut out);
        assert_eq!(hex(&out), "0300000000001e814000000000000000e03fe22d0000000000008640");
        let far = Pos::at_chunk([1 << 40, -7, i64::MAX]).unwrap().checked_add(Offset([0.125, 1023.5, 0.0])).unwrap();
        let mut out = Vec::new();
        far.encode(&mut out);
        assert_eq!(hex(&out), "808080808040000000000000c03f0d0000000000fc8f40feffffffffffffffff010000000000000000");
        let d = far - p;
        assert_eq!(
            [d.0[0].to_bits(), d.0[1].to_bits(), d.0[2].to_bits()],
            [0x4310000000001772, 0xc0b8010000000000, 0x447ffffffffffffd]
        );
        assert_eq!(
            hex(&Hash::of("pe.test", b"golden").0),
            "656fdcd17676e327ae73607c0da58259c4fd15bbc98b0dd7a1b78cd90cbcf802"
        );
    }

    #[test]
    fn contexts_separate_hashes() {
        assert_ne!(Hash::of("pe.a", b"x"), Hash::of("pe.b", b"x"));
        assert_ne!(Hash::of("pe.a", b"x"), Hash::plain(b"x"));
    }

    #[test]
    fn pos_moves_and_subtracts_exactly() {
        let p = Pos::ORIGIN.checked_add(Offset([1500.5, -0.25, 3.0])).unwrap();
        assert_eq!(p - Pos::ORIGIN, Offset([1500.5, -0.25, 3.0]));
        let far = Pos::at_chunk([1 << 40, 0, -(1 << 40)]).unwrap();
        let near = far.checked_add(Offset([0.5, 0.0, 0.0])).unwrap();
        assert_eq!(near - far, Offset([0.5, 0.0, 0.0]));
        assert_eq!(Pos::ORIGIN.checked_add(Offset([f64::NAN, 0.0, 0.0])), Err(OutOfRange));
        assert_eq!(Pos::at_chunk([i64::MIN, 0, 0]), Err(OutOfRange));
        let top = Pos::at_chunk([i64::MAX, 0, 0]).unwrap();
        assert_eq!(top.checked_add(Offset([CHUNK, 0.0, 0.0])), Err(OutOfRange));
    }
}
