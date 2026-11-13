//! The fault bundle (divergence.md 6): written on every repair past rung 1 and on every root split.
//!
//! pe-diverge fills what it knows; the embedding (pe-app) adds the build, the state at `Cv` and the entries from `Cv`
//! to `C` before writing the file. The format is versioned and decodes without panicking. On a non-admin's machine the
//! file is discarded with the world when they leave (9.6).

use bytes::Bytes;
use pe_seams::{CutRef, Frame, Hash, LeafKey, LineId, MemberKey, Record};

use crate::msg::{DecodeError, EncodeError, Reader, put_bytes, put_cut, put_key, put_record};

pub const BUNDLE_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultBundle {
    /// Engine build id, platform and CPU features, self-test result: filled by the embedding.
    pub build: String,
    pub cv: Option<(CutRef, Hash)>,
    /// The cut that diverged; its line names the world version.
    pub cut: CutRef,
    pub agreed_root: Hash,
    pub my_root: Hash,
    pub voters: Vec<MemberKey>,
    /// The differing leaves.
    pub differing: Vec<LeafKey>,
    /// Their records under my root, before the import, when my tree was trusted.
    pub mine: Vec<Record>,
    /// Their records under the agreed root.
    pub theirs: Vec<Record>,
    /// The rung that cured it (2 to 5), or 0 for a root split.
    pub rung: u8,
    pub sources: Vec<MemberKey>,
    pub nanos_taken: u64,
    /// The state at `Cv` for the cells holding the differing objects, and the entries from `Cv` to `C`, opaque here:
    /// added by the embedding.
    pub state_at_cv: Bytes,
    pub entries: Vec<Bytes>,
}

impl Default for FaultBundle {
    fn default() -> Self {
        FaultBundle {
            build: String::new(),
            cv: None,
            cut: CutRef { line: LineId::default(), height: 0, frame: Frame(0) },
            agreed_root: Hash::ZERO,
            my_root: Hash::ZERO,
            voters: Vec::new(),
            differing: Vec::new(),
            mine: Vec::new(),
            theirs: Vec::new(),
            rung: 0,
            sources: Vec::new(),
            nanos_taken: 0,
            state_at_cv: Bytes::new(),
            entries: Vec::new(),
        }
    }
}

impl FaultBundle {
    /// A file name for the bundle: the line, the cut's height and frame, and the rung (0 for a root split), so a
    /// split's bundle and a repair's at one cut never share a name (@shader-reviewer, L5).
    pub fn name(&self) -> String {
        let l = &self.cut.line.0.0;
        format!(
            "fault-{:02x}{:02x}{:02x}{:02x}-{}-{}-r{}.pefault",
            l[0], l[1], l[2], l[3], self.cut.height, self.cut.frame.0, self.rung
        )
    }

    pub fn encode(&self) -> Result<Bytes, EncodeError> {
        let mut w = vec![BUNDLE_VERSION];
        put_bytes(&mut w, self.build.as_bytes());
        match &self.cv {
            Some((c, h)) => {
                w.push(1);
                put_cut(&mut w, c);
                w.extend_from_slice(&h.0);
            }
            None => w.push(0),
        }
        put_cut(&mut w, &self.cut);
        w.extend_from_slice(&self.agreed_root.0);
        w.extend_from_slice(&self.my_root.0);
        put_keys(&mut w, &self.voters);
        w.extend_from_slice(&(self.differing.len() as u32).to_le_bytes());
        for k in &self.differing {
            put_key(&mut w, k);
        }
        for set in [&self.mine, &self.theirs] {
            w.extend_from_slice(&(set.len() as u32).to_le_bytes());
            for r in set {
                put_record(&mut w, r)?;
            }
        }
        w.push(self.rung);
        put_keys(&mut w, &self.sources);
        w.extend_from_slice(&self.nanos_taken.to_le_bytes());
        put_bytes(&mut w, &self.state_at_cv);
        w.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            put_bytes(&mut w, e);
        }
        Ok(Bytes::from(w))
    }

    pub fn decode(bytes: &[u8]) -> Result<FaultBundle, DecodeError> {
        let mut r = Reader { b: bytes };
        if r.u8()? != BUNDLE_VERSION {
            return Err(DecodeError::Version);
        }
        let build = String::from_utf8(r.bytes(usize::MAX)?.to_vec()).map_err(|_| DecodeError::Tag)?;
        let cv = match r.u8()? {
            0 => None,
            1 => Some((r.cut()?, r.hash()?)),
            _ => return Err(DecodeError::Tag),
        };
        let cut = r.cut()?;
        let agreed_root = r.hash()?;
        let my_root = r.hash()?;
        let voters = get_keys(&mut r)?;
        let n = bounded(&mut r, 2)?;
        let mut differing = Vec::with_capacity(n);
        for _ in 0..n {
            differing.push(r.key()?);
        }
        let mut sets = [Vec::new(), Vec::new()];
        for set in &mut sets {
            let n = bounded(&mut r, 9)?;
            for _ in 0..n {
                set.push(r.record()?);
            }
        }
        let [mine, theirs] = sets;
        let rung = r.u8()?;
        let sources = get_keys(&mut r)?;
        let nanos_taken = r.u64()?;
        let state_at_cv = r.bytes(usize::MAX)?;
        let n = bounded(&mut r, 4)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push(r.bytes(usize::MAX)?);
        }
        if !r.b.is_empty() {
            return Err(DecodeError::Trailing);
        }
        Ok(FaultBundle {
            build,
            cv,
            cut,
            agreed_root,
            my_root,
            voters,
            differing,
            mine,
            theirs,
            rung,
            sources,
            nanos_taken,
            state_at_cv,
            entries,
        })
    }
}

fn put_keys(w: &mut Vec<u8>, v: &[MemberKey]) {
    w.extend_from_slice(&(v.len() as u32).to_le_bytes());
    for k in v {
        w.extend_from_slice(&k.0);
    }
}

/// A u32 count of items at least `min` bytes each, refused if the rest of the input cannot hold them.
fn bounded(r: &mut Reader, min: usize) -> Result<usize, DecodeError> {
    let n = r.u32()?;
    if n.saturating_mul(min) > r.b.len() {
        return Err(DecodeError::Truncated);
    }
    Ok(n)
}

fn get_keys(r: &mut Reader) -> Result<Vec<MemberKey>, DecodeError> {
    let n = bounded(r, 32)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(MemberKey(r.hash()?.0));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{LineId, Nibbles, Node, ObjectId};

    #[test]
    fn a_bundle_round_trips_and_its_prefixes_never_panic() {
        let line = LineId(Hash([1; 32]));
        let rec = |b: &'static [u8]| Record {
            key: LeafKey::Object(ObjectId::from_u128(5)),
            bytes: Bytes::from_static(b),
            proof: vec![Node::Inner { prefix: Nibbles::default(), children: vec![(5, Hash([2; 32]))] }],
        };
        let b = FaultBundle {
            build: "pe 0.0.0 x86_64".into(),
            cv: Some((CutRef { line, height: 1, frame: Frame(8) }, Hash([2; 32]))),
            cut: CutRef { line, height: 2, frame: Frame(16) },
            agreed_root: Hash([3; 32]),
            my_root: Hash([4; 32]),
            voters: vec![MemberKey([5; 32])],
            differing: vec![LeafKey::Object(ObjectId::from_u128(5)), LeafKey::Table(1)],
            mine: vec![rec(&[1, 2])],
            theirs: vec![rec(&[3, 4])],
            rung: 2,
            sources: vec![MemberKey([5; 32])],
            nanos_taken: 3,
            state_at_cv: Bytes::from_static(&[7; 10]),
            entries: vec![Bytes::from_static(&[8; 3])],
        };
        let e = b.encode().unwrap();
        assert_eq!(FaultBundle::decode(&e), Ok(b.clone()));
        for n in 0..e.len() {
            let _ = FaultBundle::decode(&e[..n]);
        }
        assert_eq!(b.name(), "fault-01010101-2-16-r2.pefault");
    }
}
