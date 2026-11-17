//! `replay-bundle <file.pefault> [--max-frames N]` (divergence.md 6): replay a fault bundle on this machine and build.
//!
//! It imports the bundle's state at `Cv` into an empty replica over the real world (pe-replica's `PeWorld`), steps the
//! bundle's entries, and prints this build's root at every frame from `Cv` up to the cut that diverged (`C`), then
//! which root it reached at `C` and the differing objects' hashes there. Two machines' printouts, or two builds',
//! differ first at the first frame that diverged.
//!
//! Each frame's root is S2's `root_at_cut` on a cut made for that frame: `Cv`'s heads advanced by the bundle's entries
//! stamped below it (checked with @engine-architect). Each such root re-steps from the nearest keyframe
//! and holds a snapshot, so the frames are capped (`--max-frames`, 600 by default).
//!
//! Blobs are content-addressed and not in the hash tree, so a bundle does not carry them. A world whose objects name
//! blobs replays only where those blobs are in the local store ([`replay_with`]); a missing mesh is named, and the
//! replay stops [I].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pe_diverge::{CvState, FaultBundle};
use pe_replica::logic::LogicRunner;
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::{
    ChainHead, ChainId, Cut, CutImport, CutRef, EntrySource, FinalCut, Frame, GroupSel, Hash, HostTime, LeafKey,
    ObjectId, Replica, SignedEntry, SimRate,
};
use pe_world::components::Shape;
use pe_world::{BlobStore, World};

/// The default cap on the frames replayed.
pub const MAX_FRAMES: u64 = 600;

/// What a replay found.
#[derive(Debug)]
pub struct Report {
    /// `Cv`'s frame, and `C`'s.
    pub cv: u64,
    pub c: u64,
    /// This build's root at each frame from `Cv` up, or why it has none.
    pub roots: Vec<(u64, Result<Hash, String>)>,
    /// The root at `C`, when the frames reached it.
    pub at_c: Option<Hash>,
    /// The differing objects' leaf hashes at `C`.
    pub objects: Vec<(ObjectId, Option<Hash>)>,
    /// Carried entries that `C` does not include (a later cut does): left out of every frame's cut.
    pub not_in_c: usize,
}

impl Report {
    /// The first frame whose root differs between two reports, if any.
    pub fn first_difference(&self, other: &Report) -> Option<u64> {
        self.roots.iter().zip(&other.roots).find(|(a, b)| a.0 == b.0 && a.1 != b.1).map(|(a, _)| a.0)
    }
}

/// The bundle's entries, by frame in their agreed order: a replay's entry source.
struct BundleSource {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    kept_from: Frame,
}

impl EntrySource for BundleSource {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        if frame < self.kept_from {
            return None;
        }
        Some(self.entries.get(&frame).map_or(&[][..], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        self.kept_from
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

/// The cut at `frame`: `Cv`'s heads, advanced by the entries stamped below `frame`.
pub fn cut_at(cv: &Cut, entries: &[SignedEntry], c: &CutRef, frame: u64) -> Cut {
    let mut heads: BTreeMap<ChainId, ChainHead> = cv.heads.iter().map(|h| (h.chain, *h)).collect();
    for e in entries.iter().filter(|e| e.entry.stamp.0 < frame) {
        let h = ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() };
        if heads.get(&h.chain).is_none_or(|old| old.seq < h.seq) {
            heads.insert(h.chain, h);
        }
    }
    Cut { at: CutRef { line: c.line, height: c.height, frame: Frame(frame) }, heads: heads.into_values().collect() }
}

/// Replay `b` on this build, at most `max_frames` frames from `Cv`.
pub fn replay(b: &FaultBundle, max_frames: u64) -> Result<Report, String> {
    replay_with(b, max_frames, BlobStore::default())
}

/// [`replay`], with a local blob store: blobs are not in the bundle (they are content-addressed, outside the tree).
pub fn replay_with(b: &FaultBundle, max_frames: u64, blobs: BlobStore) -> Result<Report, String> {
    if b.state_at_cv.is_empty() {
        return Err("the bundle carries no state at Cv (evidence_skipped, or a root split's bundle)".into());
    }
    let state = CvState::decode(&b.state_at_cv).map_err(|e| format!("the state at Cv does not decode: {e:?}"))?;
    let entries = b
        .entries
        .iter()
        .map(|e| SignedEntry::decode(e).map_err(|e| format!("an entry does not decode: {e:?}")))
        .collect::<Result<Vec<_>, _>>()?;
    let rate = SimRate::new(state.rate_hz).ok_or(format!("the bundle's rate {} Hz is not a rate", state.rate_hz))?;
    let clock = MockClock { rate, start: HostTime(0) };
    let config = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(clock));
    let mut world = World::new(0, blobs);
    world.set_rate(rate);
    // TODO(@diverge-eng, divergence.md 6): refuse a bundle whose runner identity isn't this runner's, and load the
    // bundle's modules into it, once bundles carry them.
    let runner = Box::new(LogicRunner::with_defaults().map_err(|e| format!("no script runner: {e}"))?);
    let mut replica = ReplicaImpl::new(PeWorld::new(world, state.domain), config)
        .map_err(|e| format!("no replica: {e:?}"))?
        .with_runner(runner);
    let cv = state.cv.at.frame.0;
    let mut source = BundleSource { entries: BTreeMap::new(), kept_from: state.cv.at.frame };
    for e in &entries {
        source.entries.entry(e.entry.stamp).or_default().push(e.clone());
    }
    let import = CutImport {
        blobs: vec![],
        cut: state.cv.clone(),
        target_root: state.root,
        records: state.records.clone(),
        absent: Vec::new(),
        physics: state.physics.clone(),
    };
    match replica.import_at_cut(&GroupSel::Domain, import, &mut source) {
        Ok(root) if root == state.root => {}
        Ok(root) => return Err(format!("the import gave {root:?}, not Cv's root {:?}", state.root)),
        // A small simulation blob (a mesh of 256 bytes or less) the bundle cannot carry and this store lacks: pe-world
        // refuses the import (world-model.md 3.4), so name it as the check below names a larger one.
        Err(pe_seams::ImportError::MissingBlob(h)) => {
            let h: String = h.0.iter().map(|b| format!("{b:02x}")).collect();
            return Err(format!("blob {h} is not in the local store"));
        }
        Err(e) => return Err(format!("the state at Cv does not import: {e:?}")),
    }
    // A mesh the local store lacks would show as a false divergence: name it and stop (@design-reviewer and
    // @shader-reviewer on 59c742c7).
    let w = replica.world().world();
    for id in w.store().objects() {
        if let Some(shape) = w.store().get::<Shape>(*id)
            && !shape.mesh.is_none()
            && w.blobs().get(&shape.mesh).is_none()
        {
            let h: String = shape.mesh.hash.iter().map(|b| format!("{b:02x}")).collect();
            return Err(format!("blob {h} (object {}'s mesh) is not in the local store", id.to_u128()));
        }
    }
    // `C` as agreed, when the bundle names its heads: the carried entries can hold more, stamped below `C` and
    // included only by a later cut (@shader-reviewer on 59c742c7). Each frame's cut then takes only `C`'s entries.
    let c_cut = (!b.cut_heads.is_empty()).then(|| Cut { at: b.cut, heads: b.cut_heads.clone() });
    let effective: Vec<SignedEntry> = match &c_cut {
        Some(cc) => entries.iter().filter(|e| cc.includes(&e.entry.chain, e.entry.seq)).cloned().collect(),
        None => entries.clone(),
    };
    let not_in_c = entries.len() - effective.len();
    let c = b.cut.frame.0;
    let last = c.min(cv.saturating_add(max_frames));
    let mut roots = Vec::new();
    for f in cv..=last {
        // Step to just past `f` before asking for its root, so every root is taken near the present, whatever the
        // replica's keyframe retention (@design-reviewer on 59c742c7).
        replica.advance(Frame(f + 1), &mut source);
        let cut = match &c_cut {
            Some(cc) if f == c => cc.clone(),
            _ => cut_at(&state.cv, &effective, &b.cut, f),
        };
        roots.push((f, replica.root_at_cut(&cut, &mut source).map_err(|e| format!("{e:?}"))));
    }
    let at_c = roots.last().filter(|r| r.0 == c).and_then(|r| r.1.clone().ok());
    let ids: Vec<ObjectId> =
        b.differing.iter().filter_map(|k| if let LeafKey::Object(o) = k { Some(*o) } else { None }).collect();
    let objects = match at_c.and_then(|r| replica.tree(&r)) {
        Some(tree) => tree.object_hashes(&ids),
        None => Vec::new(),
    };
    Ok(Report { cv, c, roots, at_c, objects, not_in_c })
}

fn hex(h: &Hash) -> String {
    h.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: replay-bundle <file.pefault> [--max-frames N]");
        std::process::exit(2);
    };
    let max = match args.iter().position(|a| a == "--max-frames") {
        Some(i) => args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(MAX_FRAMES),
        None => MAX_FRAMES,
    };
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(1);
    });
    let b = FaultBundle::decode(&bytes).unwrap_or_else(|e| {
        eprintln!("not a fault bundle: {e:?}");
        std::process::exit(1);
    });
    let r = replay(&b, max).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    if r.not_in_c > 0 {
        println!(
            "note: {} carried entries are not in the cut at {} (a later cut includes them); left out",
            r.not_in_c, r.c
        );
    }
    for (f, root) in &r.roots {
        match root {
            Ok(h) => println!("frame {f} root {}", hex(h)),
            Err(e) => println!("frame {f} no root: {e}"),
        }
    }
    match r.at_c {
        Some(h) if h == b.agreed_root => println!("cut {}: this build reproduces the agreed root", r.c),
        Some(h) if h == b.my_root => println!("cut {}: this build reproduces the diverged machine's root", r.c),
        Some(h) => println!("cut {}: this build reaches neither root ({})", r.c, hex(&h)),
        None => println!("cut {}: not reached within {max} frames", r.c),
    }
    for (id, h) in &r.objects {
        match h {
            Some(h) => println!("object {} {}", id.to_u128(), hex(h)),
            None => println!("object {} absent", id.to_u128()),
        }
    }
}
