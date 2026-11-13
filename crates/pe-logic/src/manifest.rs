//! The manifest, decoded and checked (docs/spec/logic.md 3.2). The encoding is pe-sdk's [`pe_sdk::manifest`].

use pe_sdk::layout::{Access, BATCH_ALIGN, FieldKind, MANIFEST_MAGIC, MAX_BOUND, Stage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
    pub count: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub size: u32,
    pub align: u32,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct System {
    pub name: String,
    pub stage: Stage,
    pub creates: bool,
    pub bound: Vec<(String, Access)>,
    pub reads_other: Vec<String>,
    pub emits: Vec<(String, bool)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub abi: u32,
    pub sdk: u32,
    /// The batch area's address in the module's memory, and its size.
    pub batch_addr: u32,
    pub batch_cap: u32,
    pub components: Vec<Component>,
    pub systems: Vec<System>,
}

/// Why a manifest was refused: the API's `Manifest` diagnostic kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestError(pub String);

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "manifest: {}", self.0)
    }
}

impl std::error::Error for ManifestError {}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ManifestError> {
        let s = self.b.get(self.at..self.at + n).ok_or_else(|| ManifestError("truncated".into()))?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ManifestError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ManifestError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn count(&mut self, max: u32, what: &str) -> Result<u32, ManifestError> {
        let n = self.u32()?;
        if n > max { Err(ManifestError(format!("{n} {what}, more than {max}"))) } else { Ok(n) }
    }
    fn str(&mut self) -> Result<String, ManifestError> {
        let n = self.count(256, "bytes in a name")? as usize;
        let s = std::str::from_utf8(self.take(n)?).map_err(|_| ManifestError("a name is not UTF-8".into()))?;
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ManifestError(format!("{s:?} is not a name")));
        }
        Ok(s.to_string())
    }
}

/// Decode and check a manifest the module returned.
pub fn decode(b: &[u8]) -> Result<Manifest, ManifestError> {
    let mut r = Reader { b, at: 0 };
    if r.take(4)? != MANIFEST_MAGIC {
        return Err(ManifestError("bad magic".into()));
    }
    let abi = r.u32()?;
    let sdk = r.u32()?;
    let batch_addr = r.u32()?;
    let batch_cap = r.u32()?;
    if !(batch_addr as usize).is_multiple_of(BATCH_ALIGN) {
        return Err(ManifestError("the batch area is not 16-aligned".into()));
    }
    let mut components = Vec::new();
    for _ in 0..r.count(256, "components")? {
        let name = r.str()?;
        let size = r.u32()?;
        let align = r.u32()?;
        let mut fields = Vec::new();
        for _ in 0..r.count(256, "fields")? {
            let fname = r.str()?;
            let kind =
                FieldKind::from_code(r.u8()?).ok_or_else(|| ManifestError(format!("{name}.{fname}: unknown kind")))?;
            let count = r.u32()?;
            let offset = r.u32()?;
            fields.push(Field { name: fname, kind, count, offset });
        }
        check_layout(&name, size, align, &fields)?;
        if components.iter().any(|c: &Component| c.name == name) {
            return Err(ManifestError(format!("component {name} declared twice")));
        }
        components.push(Component { name, size, align, fields });
    }
    let mut systems = Vec::new();
    for _ in 0..r.count(64, "systems")? {
        let name = r.str()?;
        let stage = match r.u8()? {
            1 => Stage::PrePhysics,
            2 => Stage::PostPhysics,
            s => return Err(ManifestError(format!("system {name}: unknown stage {s}"))),
        };
        let creates = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(ManifestError(format!("system {name}: creates is not a boolean"))),
        };
        let mut bound = Vec::new();
        for _ in 0..r.count(MAX_BOUND as u32, "bound components")? {
            let c = r.str()?;
            let a = match r.u8()? {
                0 => Access::Read,
                1 => Access::Write,
                _ => return Err(ManifestError(format!("system {name}: bad access"))),
            };
            if bound.iter().any(|(b, _): &(String, Access)| *b == c) {
                return Err(ManifestError(format!("system {name} binds {c} twice")));
            }
            bound.push((c, a));
        }
        let mut reads_other = Vec::new();
        for _ in 0..r.count(64, "reads")? {
            reads_other.push(r.str()?);
        }
        let mut emits = Vec::new();
        for _ in 0..r.count(64, "events")? {
            let e = r.str()?;
            let per_player = r.u8()? == 1;
            emits.push((e, per_player));
        }
        if systems.iter().any(|s: &System| s.name == name) {
            return Err(ManifestError(format!("system {name} declared twice")));
        }
        systems.push(System { name, stage, creates, bound, reads_other, emits });
    }
    if r.at != b.len() {
        return Err(ManifestError("trailing bytes".into()));
    }
    Ok(Manifest { abi, sdk, batch_addr, batch_cap, components, systems })
}

/// The store's layout rules, checked here so a bad declaration is refused at submission: fields inside the row,
/// aligned to their kind, not overlapping, and no padding (a row is exactly its fields, so its bytes are defined).
fn check_layout(name: &str, size: u32, align: u32, fields: &[Field]) -> Result<(), ManifestError> {
    let bad = |m: String| Err(ManifestError(format!("component {name}: {m}")));
    if !align.is_power_of_two() || align > 16 || !size.is_multiple_of(align) {
        return bad(format!("size {size} and alignment {align} do not fit"));
    }
    let mut spans: Vec<(u32, u32, &str)> = Vec::new();
    for f in fields {
        let el = f.kind.size();
        let a = el.min(8);
        let len = el.checked_mul(f.count).filter(|&l| l > 0);
        let Some(len) = len else { return bad(format!("{}: count {}", f.name, f.count)) };
        if f.offset % a != 0 || f.offset.checked_add(len).is_none_or(|e| e > size) {
            return bad(format!("{}: offset {} does not fit", f.name, f.offset));
        }
        spans.push((f.offset, f.offset + len, &f.name));
    }
    spans.sort();
    let mut at = 0;
    for (a, b, n) in &spans {
        if *a != at {
            return bad(if *a < at { format!("{n} overlaps") } else { format!("padding before {n}") });
        }
        at = *b;
    }
    if at != size {
        return bad(format!("padding at the end ({} of {size} bytes used)", at));
    }
    Ok(())
}
