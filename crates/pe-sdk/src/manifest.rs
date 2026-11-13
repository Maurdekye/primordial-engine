//! The manifest (docs/spec/logic.md 3.2): what a script declares, encoded little-endian into a fixed buffer (v0 has
//! no allocator). pe-logic decodes it with [`crate::layout`]'s definitions.
//!
//! Encoding, all integers little-endian, a string is `u32 len` then UTF-8 bytes:
//! - `MANIFEST_MAGIC`, `u32 abi`, `u32 sdk`, `u32 batch_addr`, `u32 batch_cap`;
//! - `u32` components, each: name, `u32 size`, `u32 align`, `u32` fields, each: name, `u8 kind`, `u32 count`,
//!   `u32 offset`;
//! - `u32` systems, each: name, `u8 stage`, `u8 creates`, `u32` bound, each: component name, `u8 access`; `u32`
//!   reads_other, each a component name; `u32` emits, each: event name, `u8 once_per_player`.

use crate::component::ComponentInfo;
use crate::layout::{Access, MANIFEST_MAGIC, SDK_VERSION, Stage};

/// One system's declaration.
#[derive(Clone, Copy, Debug)]
pub struct SystemInfo {
    pub name: &'static str,
    pub stage: Stage,
    /// Components every bound object has, read or written, in the batch's column order.
    pub bound: &'static [(&'static str, Access)],
    pub reads_other: &'static [&'static str],
    /// Event types it may emit, and whether each is once per player (else once for the world).
    pub emits: &'static [(&'static str, bool)],
    pub creates: bool,
}

/// Writes into a byte slice; reports overflow instead of panicking.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pub len: usize,
    pub overflow: bool,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, len: 0, overflow: false }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        match self.buf.get_mut(self.len..self.len + b.len()) {
            Some(d) => {
                d.copy_from_slice(b);
                self.len += b.len();
            }
            None => self.overflow = true,
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
    }
}

/// Encode a manifest into `buf`; returns its length, or `None` if it does not fit.
pub fn encode(
    buf: &mut [u8],
    abi: u32,
    batch: (u32, u32),
    components: &[ComponentInfo],
    systems: &[SystemInfo],
) -> Option<usize> {
    let mut w = Writer::new(buf);
    w.bytes(&MANIFEST_MAGIC);
    w.u32(abi);
    w.u32(SDK_VERSION);
    w.u32(batch.0);
    w.u32(batch.1);
    w.u32(components.len() as u32);
    for c in components {
        w.str(c.name);
        w.u32(c.size);
        w.u32(c.align);
        w.u32(c.fields.len() as u32);
        for f in c.fields {
            w.str(f.name);
            w.u8(f.kind as u8);
            w.u32(f.count);
            w.u32(f.offset);
        }
    }
    w.u32(systems.len() as u32);
    for s in systems {
        w.str(s.name);
        w.u8(s.stage as u8);
        w.u8(s.creates as u8);
        w.u32(s.bound.len() as u32);
        for (name, a) in s.bound {
            w.str(name);
            w.u8(*a as u8);
        }
        w.u32(s.reads_other.len() as u32);
        for name in s.reads_other {
            w.str(name);
        }
        w.u32(s.emits.len() as u32);
        for (name, per_player) in s.emits {
            w.str(name);
            w.u8(*per_player as u8);
        }
    }
    if w.overflow { None } else { Some(w.len) }
}
