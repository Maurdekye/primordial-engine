//! The source archive's check (logic.md 4.1a; world-model.md 7.9). `check_source` is consensus code: these tests pin
//! what it accepts and refuses on exact bytes (@coordinator-opus m043445294e6a), so a change to either side shows.

use pe_world::script::{MAX_SOURCE_BYTES, SourceError, check_source, pack_source, path_ok, source_files};

/// Raw archive bytes: the magic, then each `(path, body)` as given, in the order given, unchecked.
fn raw(files: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = b"pe.src.v0".to_vec();
    for (path, body) in files {
        for part in [*path, *body] {
            out.extend_from_slice(&(part.len() as u32).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    out
}

const LIB: (&[u8], &[u8]) = (b"src/lib.rs", b"pub fn f() {}\n");

#[test]
fn the_smallest_archive_is_exactly_these_bytes() {
    let bytes: &[u8] = b"pe.src.v0\x0a\x00\x00\x00src/lib.rs\x00\x00\x00\x00";
    assert_eq!(check_source(bytes), Ok(()));
    assert_eq!(raw(&[(b"src/lib.rs", b"")]), bytes);
    assert_eq!(pack_source(&[("src/lib.rs", b"")]).unwrap(), bytes);
}

#[test]
fn files_come_in_byte_order_of_their_paths() {
    let ok = raw(&[(b"src/a.rs", b"x"), (b"src/a/b.rs", b"y"), LIB]);
    // "src/a.rs" < "src/a/b.rs" ('.' 0x2e < '/' 0x2f) < "src/lib.rs".
    assert_eq!(check_source(&ok), Ok(()));
    let files = source_files(&ok).unwrap();
    assert_eq!(files.iter().map(|f| f.0).collect::<Vec<_>>(), ["src/a.rs", "src/a/b.rs", "src/lib.rs"]);
    // pack_source sorts.
    assert_eq!(pack_source(&[("src/lib.rs", LIB.1), ("src/a/b.rs", b"y"), ("src/a.rs", b"x")]).unwrap(), ok);

    let unsorted = raw(&[LIB, (b"src/a.rs", b"x")]);
    assert_eq!(check_source(&unsorted), Err(SourceError::Order("src/a.rs".into())));
    let duplicate = raw(&[LIB, LIB]);
    assert_eq!(check_source(&duplicate), Err(SourceError::Order("src/lib.rs".into())));
}

#[test]
fn a_case_collision_pair_is_refused() {
    // `src/A.rs` and `src/a.rs` are one file on Windows and macOS: upper case is refused outright, so the pair can
    // never be in one archive.
    let pair = raw(&[(b"src/A.rs", b"x"), (b"src/a.rs", b"y"), LIB]);
    assert_eq!(check_source(&pair), Err(SourceError::Path("src/A.rs".into())));
    assert_eq!(check_source(&raw(&[(b"src/Lib.rs", b""), LIB])), Err(SourceError::Path("src/Lib.rs".into())));
}

#[test]
fn only_portable_paths_are_accepted() {
    for good in ["src/lib.rs", "src/a_1.rs", "src/net/peer_2.rs", "src/con_x.rs", "src/com10.rs"] {
        assert!(path_ok(good), "{good}");
    }
    for bad in [
        "Cargo.toml",    // written by the build service (4.1a)
        "build.rs",      // outside src/
        "src/../lib.rs", // `..`
        "src/./lib.rs",  // `.`
        "src/lib.RS",    // the ending is `.rs`
        "src/lib",       // no `.rs`
        "src/.rs",       // a bare `.rs`: the stem is at least one byte
        "src//lib.rs",   // an empty segment
        "src/a.b.rs",    // `.` inside a stem
        "src/a-b.rs",    // `-`
        "src/a b.rs",    // a space
        "src/lib.rs/",   // a trailing `/`
        "src\\lib.rs",   // `\`
        "src/con.rs",    // reserved stems
        "src/aux/x.rs",
        "src/nul.rs",
        "src/com1.rs",
        "src/lpt9.rs",
        "src/caf\u{e9}.rs", // non-ASCII
        "/src/lib.rs",      // absolute
    ] {
        assert!(!path_ok(bad), "{bad}");
    }
    let non_ascii = raw(&[(b"src/caf\xc3\xa9.rs", b""), LIB]);
    assert_eq!(check_source(&non_ascii), Err(SourceError::Path("src/caf\u{e9}.rs".into())));
    let dotdot = raw(&[(b"src/../x.rs", b""), LIB]);
    assert_eq!(check_source(&dotdot), Err(SourceError::Path("src/../x.rs".into())));
    let reserved = raw(&[(b"src/aux.rs", b""), LIB]);
    assert_eq!(check_source(&reserved), Err(SourceError::Path("src/aux.rs".into())));
    let manifest = raw(&[(b"Cargo.toml", b"[package]\n"), LIB]);
    assert_eq!(check_source(&manifest), Err(SourceError::Path("Cargo.toml".into())));
}

#[test]
fn a_segment_is_at_most_64_bytes_with_its_rs() {
    let stem = |n: usize| "a".repeat(n);
    assert!(path_ok(&format!("src/{}.rs", stem(61))));
    assert!(!path_ok(&format!("src/{}.rs", stem(62))));
    assert!(path_ok(&format!("src/{}/lib.rs", stem(64))));
    assert!(!path_ok(&format!("src/{}/lib.rs", stem(65))));
}

#[test]
fn malformed_archives_are_refused() {
    assert_eq!(check_source(b""), Err(SourceError::Magic));
    assert_eq!(check_source(b"pe.src.v1"), Err(SourceError::Magic));
    // No files: no lib.rs.
    assert_eq!(check_source(b"pe.src.v0"), Err(SourceError::Missing("src/lib.rs")));
    assert_eq!(check_source(&raw(&[(b"src/a.rs", b"")])), Err(SourceError::Missing("src/lib.rs")));
    // A length past the end, a cut length, and a path without its body.
    let full = raw(&[LIB]);
    for cut in [full.len() - 1, 9 + 2, 9 + 4 + 10] {
        assert_eq!(check_source(&full[..cut]), Err(SourceError::Truncated), "cut at {cut}");
    }
    let huge = [b"pe.src.v0".as_slice(), &u32::MAX.to_le_bytes()].concat();
    assert_eq!(check_source(&huge), Err(SourceError::Truncated));
    // A body that is not UTF-8.
    assert_eq!(check_source(&raw(&[(b"src/lib.rs", b"\xff")])), Err(SourceError::NotUtf8("src/lib.rs".into())));
}

#[test]
fn an_archive_over_the_cap_is_refused_and_one_at_it_accepted() {
    let cap = MAX_SOURCE_BYTES as usize;
    assert_eq!(cap, 262_144);
    // The magic (9), lib.rs's two lengths (8) and path (10): the body takes the rest.
    let at = raw(&[(b"src/lib.rs", "/".repeat(cap - 27).as_bytes())]);
    assert_eq!(at.len(), cap);
    assert_eq!(check_source(&at), Ok(()));
    let over = raw(&[(b"src/lib.rs", "/".repeat(cap - 26).as_bytes())]);
    assert_eq!(check_source(&over), Err(SourceError::TooLarge));
}
