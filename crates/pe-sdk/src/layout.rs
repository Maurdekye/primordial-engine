//! The binary layout shared by a script (the guest) and pe-logic (the host): the batch header, the field kinds, the
//! command records and the manifest's encoding (docs/spec/logic.md 3.2, 3.3). Both sides use these definitions, so
//! there is one source of truth.

/// The manifest's first four bytes.
pub const MANIFEST_MAGIC: [u8; 4] = *b"PEM1";

/// pe-sdk's version, recorded in the manifest and pinned under the SDK hash in `CommitScript`.
pub const SDK_VERSION: u32 = 1;

/// The most components one system may bind.
pub const MAX_BOUND: usize = 8;

/// Alignment of the batch area and of every column in it.
pub const BATCH_ALIGN: usize = 16;

/// The batch header, at the start of the batch area. Offsets are from the start of the area.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchHeader {
    pub frame: u64,
    /// The domain's seed.
    pub seed: u64,
    /// The world's simulation rate.
    pub rate_hz: u32,
    /// Rows in this batch.
    pub n: u32,
    /// Offset of the `n` object ids (16 bytes each), sorted by id.
    pub ids: u32,
    /// Offset and capacity in bytes of the command buffer; `cmd_len` is what the guest wrote.
    pub cmd: u32,
    pub cmd_cap: u32,
    pub cmd_len: u32,
    /// Offset of each bound component's column, in the order of the system's `bound` list.
    pub columns: [u32; MAX_BOUND],
}

pub const HEADER_BYTES: usize = core::mem::size_of::<BatchHeader>();

impl BatchHeader {
    /// The header's bytes: its fields in order, little-endian (repr(C) of integers has no padding).
    pub fn to_bytes(&self) -> [u8; HEADER_BYTES] {
        let mut b = [0u8; HEADER_BYTES];
        b[0..8].copy_from_slice(&self.frame.to_le_bytes());
        b[8..16].copy_from_slice(&self.seed.to_le_bytes());
        let words = [self.rate_hz, self.n, self.ids, self.cmd, self.cmd_cap, self.cmd_len];
        for (k, w) in words.iter().chain(self.columns.iter()).enumerate() {
            b[16 + 4 * k..20 + 4 * k].copy_from_slice(&w.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8; HEADER_BYTES]) -> BatchHeader {
        let w = |k: usize| u32::from_le_bytes(b[16 + 4 * k..20 + 4 * k].try_into().unwrap());
        let mut columns = [0u32; MAX_BOUND];
        for (k, c) in columns.iter_mut().enumerate() {
            *c = w(6 + k);
        }
        BatchHeader {
            frame: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            seed: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            rate_hz: w(0),
            n: w(1),
            ids: w(2),
            cmd: w(3),
            cmd_cap: w(4),
            cmd_len: w(5),
            columns,
        }
    }
}

/// Where `cmd_len` sits in the header: the host reads it back after a call.
pub const CMD_LEN_AT: usize = core::mem::offset_of!(BatchHeader, cmd_len);

/// The kind of a component field, as the store knows it (pe-sim `FieldKind`, plus `Pos` from WS-world, logic.md 9).
/// Codes stay below 32: the manifest's field byte keeps a colour's space in its high three bits ([`field_byte`]).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    U8 = 1,
    U16 = 2,
    U32 = 3,
    U64 = 4,
    I8 = 5,
    I16 = 6,
    I32 = 7,
    I64 = 8,
    /// Must be finite.
    F32 = 9,
    /// Must be finite.
    F64 = 10,
    /// One byte, 0 or 1.
    Bool = 11,
    /// An object id, 16 bytes (pe-seams `ObjectId`).
    ObjectId = 12,
    /// A position: three `i64` chunk indices and three `f64` offsets, 48 bytes (logic.md 8).
    Pos = 13,
}

impl FieldKind {
    pub const fn size(self) -> u32 {
        match self {
            FieldKind::U8 | FieldKind::I8 | FieldKind::Bool => 1,
            FieldKind::U16 | FieldKind::I16 => 2,
            FieldKind::U32 | FieldKind::I32 | FieldKind::F32 => 4,
            FieldKind::U64 | FieldKind::I64 | FieldKind::F64 => 8,
            FieldKind::ObjectId => 16,
            FieldKind::Pos => 48,
        }
    }

    pub const fn from_code(c: u8) -> Option<FieldKind> {
        Some(match c {
            1 => FieldKind::U8,
            2 => FieldKind::U16,
            3 => FieldKind::U32,
            4 => FieldKind::U64,
            5 => FieldKind::I8,
            6 => FieldKind::I16,
            7 => FieldKind::I32,
            8 => FieldKind::I64,
            9 => FieldKind::F32,
            10 => FieldKind::F64,
            11 => FieldKind::Bool,
            12 => FieldKind::ObjectId,
            13 => FieldKind::Pos,
            _ => return None,
        })
    }
}

/// The colour space of a field that holds a colour (pe-sim `ColorSpace`; world-model.md 3.6). The only one is linear
/// light with sRGB (Rec. 709) primaries and straight alpha in a fourth element. It changes nothing about the floats;
/// it tells a reader, the WorldApi among them, how they are meant. Only on 3 or 4 `F32` elements.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    LinearSrgb = 1,
}

impl ColorSpace {
    pub const fn from_code(c: u8) -> Option<ColorSpace> {
        match c {
            1 => Some(ColorSpace::LinearSrgb),
            _ => None,
        }
    }
}

/// A field's kind and colour space as the manifest's one byte, as pe-world's kind byte holds them (world-model.md
/// 6.6): the kind's code in the low five bits, the space's code (0 for none) in the high three. An unmarked field's
/// byte is its kind's code alone, as before the mark.
pub const fn field_byte(kind: FieldKind, color: Option<ColorSpace>) -> u8 {
    assert!((kind as u8) < 32, "a kind code takes five bits of the field byte");
    let space = match color {
        Some(c) => c as u8,
        None => 0,
    };
    kind as u8 | space << 5
}

/// Splits a [`field_byte`]; `None` for an unknown kind or space.
pub const fn split_field_byte(b: u8) -> Option<(FieldKind, Option<ColorSpace>)> {
    let Some(kind) = FieldKind::from_code(b & 0x1f) else { return None };
    let color = match b >> 5 {
        0 => None,
        c => match ColorSpace::from_code(c) {
            Some(s) => Some(s),
            None => return None,
        },
    };
    Some((kind, color))
}

/// A frame stage a system runs in (pe-seams `Stage`, N1). Only the two logic stages take systems.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    PrePhysics = 1,
    PostPhysics = 2,
}

/// How a system's bound component is accessed.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read = 0,
    Write = 1,
}

/// The kinds of command record (3.3, step 3). A record is `kind: u32, len: u32`, then `len` payload bytes, padded to
/// 8 bytes.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Payload: the creating row's index in the batch (u32), the template component's name length (u32) and bytes,
    /// then its row. The new object's id is derived by the host from the creating object, the frame and how many
    /// objects that object has created in the frame (pe-core `ObjectId::spawned`), so it is the same on every
    /// machine.
    Create = 1,
    /// Payload: the object id (16 bytes).
    Destroy = 2,
    /// Payload: the emitting row's index in the batch (u32), the event type's index in the system's `emits` list
    /// (u32), then its bytes.
    Emit = 3,
}

pub const COMMAND_HEADER: usize = 8;

/// Status codes `pe_run` returns; any other non-zero value is the script's own.
pub mod status {
    pub const OK: i32 = 0;
    /// The system index is not in the manifest.
    pub const NO_SYSTEM: i32 = -1;
    /// The batch header names a column outside the batch area, or a misaligned one.
    pub const BAD_BATCH: i32 = -2;
    /// The command buffer is full.
    pub const COMMANDS_FULL: i32 = -3;
}
