//! The guest's side of a batch (docs/spec/logic.md 3.3): the batch area, column access, the per-row context with
//! its commands, and what [`script!`](crate::script!) exports. All of the SDK's `unsafe` is in this file; a script
//! crate is `forbid(unsafe_code)` (4.1). The host trusts none of it: a script can still reach [`Batch::new`] with a
//! declaration of its own, so pe-logic treats only the columns it compared after a call as unchanged (3.3a (c)).

use crate::component::{Component, ObjectId};
use crate::layout::{Access, BATCH_ALIGN, BatchHeader, COMMAND_HEADER, Command, HEADER_BYTES, MAX_BOUND, status};
use crate::manifest::{self, SystemInfo};

/// The batch area's size. The host splits a system's objects into batches that fit.
pub const BATCH_BYTES: usize = 256 << 10;

#[repr(C, align(16))]
struct Area([u8; BATCH_BYTES]);

static mut AREA: Area = Area([0; BATCH_BYTES]);

const MANIFEST_BYTES: usize = 16 << 10;
static mut MANIFEST: [u8; MANIFEST_BYTES] = [0; MANIFEST_BYTES];

/// One system: its declaration and the function that runs it over a batch.
#[derive(Clone, Copy)]
pub struct System {
    pub info: SystemInfo,
    pub run: fn(BatchArea<'_>) -> i32,
}

/// A batch: the header, the object ids and the bound columns, checked to lie inside the area without overlapping.
pub struct Batch<'a> {
    area: *mut u8,
    len: usize,
    header: BatchHeader,
    info: &'a SystemInfo,
    sizes: [u32; MAX_BOUND],
    /// The written columns already handed out, one bit each: each goes out once.
    taken: u32,
    _area: core::marker::PhantomData<&'a mut [u8]>,
}

fn region(off: u32, bytes: u64, len: usize) -> Option<(u64, u64)> {
    let a = off as u64;
    let b = a.checked_add(bytes)?;
    (a >= HEADER_BYTES as u64 && b <= len as u64).then_some((a, b))
}

impl<'a> Batch<'a> {
    /// Check the header against the area and the system's bound components (their row sizes in `sizes`).
    pub fn new(area: BatchArea<'a>, info: &'a SystemInfo, sizes: &[u32]) -> Result<Batch<'a>, i32> {
        let area = area.0;
        if area.len() < HEADER_BYTES
            || !(area.as_ptr() as usize).is_multiple_of(BATCH_ALIGN)
            || sizes.len() != info.bound.len()
        {
            return Err(status::BAD_BATCH);
        }
        let header = BatchHeader::from_bytes(area[..HEADER_BYTES].try_into().expect("checked above"));
        let n = header.n as u64;
        let mut regions: [(u64, u64); MAX_BOUND + 2] = [(0, 0); MAX_BOUND + 2];
        regions[0] = region(header.ids, n * 16, area.len()).ok_or(status::BAD_BATCH)?;
        regions[1] = region(header.cmd, header.cmd_cap as u64, area.len()).ok_or(status::BAD_BATCH)?;
        let mut sz = [0u32; MAX_BOUND];
        for (k, &s) in sizes.iter().enumerate() {
            let off = header.columns[k];
            if !(off as usize).is_multiple_of(BATCH_ALIGN) {
                return Err(status::BAD_BATCH);
            }
            regions[2 + k] = region(off, n * s as u64, area.len()).ok_or(status::BAD_BATCH)?;
            sz[k] = s;
        }
        let used = &regions[..2 + sizes.len()];
        for (i, a) in used.iter().enumerate() {
            for b in &used[i + 1..] {
                if a.0 < b.1 && b.0 < a.1 {
                    return Err(status::BAD_BATCH);
                }
            }
        }
        if !(header.ids as usize).is_multiple_of(4) || !(header.cmd as usize).is_multiple_of(8) {
            return Err(status::BAD_BATCH);
        }
        Ok(Batch {
            area: area.as_mut_ptr(),
            len: area.len(),
            header,
            info,
            sizes: sz,
            taken: 0,
            _area: core::marker::PhantomData,
        })
    }

    pub fn n(&self) -> usize {
        self.header.n as usize
    }

    fn index<C: Component>(&self, access: Access) -> Result<usize, i32> {
        let k = self.info.bound.iter().position(|(name, _)| *name == C::NAME).ok_or(status::BAD_BATCH)?;
        if self.info.bound[k].1 != access || self.sizes[k] != C::SIZE {
            return Err(status::BAD_BATCH);
        }
        Ok(k)
    }

    /// The column of a component the system reads, as a shared slice.
    pub fn read<C: Component>(&self) -> Result<&'a [C], i32> {
        let k = self.index::<C>(Access::Read)?;
        let off = self.header.columns[k] as usize;
        // SAFETY: `new` checked that [off, off + n * size) is inside the area, 16-aligned (C's alignment is at most
        // 16 for the field kinds), and overlaps no other column, the ids or the commands. The host wrote valid rows
        // (it validates every row the store holds). The area is borrowed for 'a.
        Ok(unsafe { core::slice::from_raw_parts(self.area.add(off).cast::<C>(), self.n()) })
    }

    /// The column of a component the system writes. Each goes out once; a second request is refused.
    pub fn write<C: Component>(&mut self) -> Result<&'a mut [C], i32> {
        let k = self.index::<C>(Access::Write)?;
        if self.taken & (1 << k) != 0 {
            return Err(status::BAD_BATCH);
        }
        self.taken |= 1 << k;
        let off = self.header.columns[k] as usize;
        // SAFETY: as for `read`; the bound names are distinct (the system! macro's list, checked by the host), so
        // no two columns overlap, and `taken` hands each written column out once, so no two slices alias.
        Ok(unsafe { core::slice::from_raw_parts_mut(self.area.add(off).cast::<C>(), self.n()) })
    }

    /// The context for row `row`; commands go into the batch's command buffer.
    pub fn ctx(&mut self, row: usize) -> Ctx<'_, 'a> {
        assert!(row < self.n(), "row out of range");
        let ids = self.header.ids as usize;
        let mut id = [0u8; 16];
        // SAFETY: the ids region was checked in `new`, and row < n.
        let src = unsafe { core::slice::from_raw_parts(self.area.add(ids + row * 16), 16) };
        id.copy_from_slice(src);
        Ctx { batch: self, me: ObjectId(id), row: row as u32 }
    }

    fn push_command(&mut self, kind: Command, parts: &[&[u8]]) -> bool {
        let payload: usize = parts.iter().map(|p| p.len()).sum();
        let rec = (COMMAND_HEADER + payload).div_ceil(8) * 8;
        let at = self.header.cmd_len as usize;
        if at + rec > self.header.cmd_cap as usize {
            return false;
        }
        let base = self.header.cmd as usize + at;
        debug_assert!(base + rec <= self.len);
        // SAFETY: inside the command region checked in `new`, which overlaps no column.
        let dst = unsafe { core::slice::from_raw_parts_mut(self.area.add(base), rec) };
        dst[..4].copy_from_slice(&(kind as u32).to_le_bytes());
        dst[4..8].copy_from_slice(&(payload as u32).to_le_bytes());
        let mut p = 8;
        for part in parts {
            dst[p..p + part.len()].copy_from_slice(part);
            p += part.len();
        }
        dst[p..].fill(0);
        self.header.cmd_len += rec as u32;
        true
    }

    /// Write the command length back into the header, for the host.
    pub fn finish(&mut self) {
        let off = crate::layout::CMD_LEN_AT;
        // SAFETY: the header is the first HEADER_BYTES of the area.
        let dst = unsafe { core::slice::from_raw_parts_mut(self.area.add(off), 4) };
        dst.copy_from_slice(&self.header.cmd_len.to_le_bytes());
    }
}

/// What a system's body may ask for one row.
pub struct Ctx<'c, 'a> {
    batch: &'c mut Batch<'a>,
    me: ObjectId,
    row: u32,
}

/// The status a command that did not fit leaves; the call faults with it (3.6).
pub struct Full;

impl Ctx<'_, '_> {
    pub fn frame(&self) -> u64 {
        self.batch.header.frame
    }

    pub fn rate_hz(&self) -> u32 {
        self.batch.header.rate_hz
    }

    /// The simulation time at this frame, in seconds.
    pub fn time_s(&self) -> f32 {
        let r = self.batch.header.rate_hz.max(1) as u64;
        (self.batch.header.frame / r) as f32 + (self.batch.header.frame % r) as f32 / r as f32
    }

    /// The bound object.
    pub fn me(&self) -> ObjectId {
        self.me
    }

    /// 64 bits of the domain's stream for (frame, system, this object, `stream`). A pure function: the same stream
    /// gives the same bits within a frame, so take each draw from its own stream number.
    pub fn rand(&mut self, stream: u32) -> u64 {
        host::rand(&self.me.0, stream)
    }

    /// Create an object from a template component's row (3.3 step 3, the `creates` command; 3.3a (a)).
    pub fn create<C: Component>(&mut self, row: &C) -> Result<(), Full> {
        let name = C::NAME.as_bytes();
        // SAFETY: C is a repr(C) component of plain field types; reading its bytes is reading initialized memory
        // except padding, which component! layouts do not have (the store refuses padded layouts).
        let bytes = unsafe { core::slice::from_raw_parts((row as *const C).cast::<u8>(), C::SIZE as usize) };
        let len = (name.len() as u32).to_le_bytes();
        let row_index = self.row.to_le_bytes();
        if self.batch.push_command(Command::Create, &[&row_index, &len, name, bytes]) { Ok(()) } else { Err(Full) }
    }

    pub fn destroy(&mut self, id: ObjectId) -> Result<(), Full> {
        if self.batch.push_command(Command::Destroy, &[&id.0]) { Ok(()) } else { Err(Full) }
    }

    /// Emit event `index` of the system's `emits` list with its bytes.
    pub fn emit(&mut self, index: u32, bytes: &[u8]) -> Result<(), Full> {
        let parts: [&[u8]; 3] = [&self.row.to_le_bytes(), &index.to_le_bytes(), bytes];
        if self.batch.push_command(Command::Emit, &parts) { Ok(()) } else { Err(Full) }
    }
}

/// The host functions (ABI v0, pe-logic `abi`). Off wasm32 they are stand-ins, for the SDK's own tests.
mod host {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "pe")]
    unsafe extern "C" {
        #[link_name = "rand"]
        fn pe_rand(object: i32, stream: i32) -> i64;
    }

    /// 64 bits for (frame, system, `object`, `stream`); the host keys the stream, the guest only names the object.
    #[cfg(target_arch = "wasm32")]
    pub fn rand(object: &[u8; 16], stream: u32) -> u64 {
        // SAFETY: a host function of ABI v0 with this exact signature; it reads the 16 bytes at `object`.
        unsafe { pe_rand(object.as_ptr() as usize as i32, stream as i32) as u64 }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn rand(object: &[u8; 16], stream: u32) -> u64 {
        u64::from_le_bytes(object[..8].try_into().unwrap()) ^ stream as u64
    }
}

/// `pe_manifest()`: encode into a static buffer and return its pointer (low 32 bits) and length (high 32).
pub fn manifest(components: &[crate::component::ComponentInfo], systems: &[System]) -> i64 {
    if systems.len() > 64 {
        return 0;
    }
    let mut list = [SystemInfo {
        name: "",
        stage: crate::layout::Stage::PrePhysics,
        bound: &[],
        reads_other: &[],
        emits: &[],
        creates: false,
    }; 64];
    for (i, s) in systems.iter().enumerate() {
        list[i] = s.info;
    }
    // SAFETY: wasm32 runs one thread; the manifest buffer is written only here and read by the host after return.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(MANIFEST) };
    let area = core::ptr::addr_of!(AREA) as usize as u32;
    match manifest::encode(
        buf,
        crate::ABI_VERSION as u32,
        (area, BATCH_BYTES as u32),
        components,
        &list[..systems.len()],
    ) {
        Some(len) => (buf.as_ptr() as usize as u32 as i64) | ((len as i64) << 32),
        None => 0,
    }
}

/// `pe_run(system, batch)`: run one system over the batch the host wrote into the area.
pub fn run(systems: &[System], system: i32, _batch: i32) -> i32 {
    let Some(s) = usize::try_from(system).ok().and_then(|i| systems.get(i)) else { return status::NO_SYSTEM };
    // SAFETY: wasm32 runs one thread and the host writes the area only between calls.
    let area: &mut [u8] = unsafe { &mut (&mut *core::ptr::addr_of_mut!(AREA)).0 };
    (s.run)(BatchArea(area))
}

/// The batch area handed to a system's generated runner. Only [`Batch::new`] opens it.
pub struct BatchArea<'a>(&'a mut [u8]);

/// Declares a system: its stage, the components it reads and writes on each bound object, and its body, which runs
/// once per bound object with `ctx` ([`Ctx`]) and `me` (a row with a shared reference per read component and a
/// mutable one per written component).
///
/// Optional, in this order after `write`: `creates: true` lets the body create and destroy objects; `emits: ["name",
/// ...]` lists the event types it may emit (once for the world), which `ctx.emit` names by index.
///
/// ```ignore
/// pe_sdk::system! {
///     fn pulse(stage: PrePhysics, read: [blink: Blink], write: [light: Light]) |ctx, me| {
///         me.light.intensity = if (ctx.time_s() / me.blink.period).fract() < 0.5 { 1.0 } else { 0.0 };
///     }
/// }
/// ```
#[macro_export]
macro_rules! system {
    ($(#[$m:meta])* fn $name:ident (stage: $stage:ident, read: [$($rn:ident : $rt:ty),* $(,)?], write: [$($wn:ident : $wt:ty),* $(,)?] $(, creates: $creates:literal)? $(, emits: [$($ev:literal),* $(,)?])?) |$ctx:ident, $me:ident| $body:block) => {
        $(#[$m])*
        #[allow(non_snake_case)]
        pub mod $name {
            #[allow(unused_imports)]
            use super::*;
            use $crate::component::Component as _;

            /// One bound object's components.
            pub struct Row<'r> {
                $(pub $rn: &'r $rt,)*
                $(pub $wn: &'r mut $wt,)*
            }

            pub const SYSTEM: $crate::guest::System = $crate::guest::System {
                info: $crate::manifest::SystemInfo {
                    name: stringify!($name),
                    stage: $crate::layout::Stage::$stage,
                    bound: &[
                        $((<$rt as $crate::component::Component>::NAME, $crate::layout::Access::Read),)*
                        $((<$wt as $crate::component::Component>::NAME, $crate::layout::Access::Write),)*
                    ],
                    reads_other: &[],
                    emits: &[$($(($ev, false),)*)?],
                    creates: false $(|| $creates)?,
                },
                run,
            };

            const SIZES: &[u32] = &[
                $(<$rt as $crate::component::Component>::SIZE,)*
                $(<$wt as $crate::component::Component>::SIZE,)*
            ];

            fn run(area: $crate::guest::BatchArea<'_>) -> i32 {
                let info = &SYSTEM.info;
                let mut batch = match $crate::guest::Batch::new(area, info, SIZES) {
                    Ok(b) => b,
                    Err(s) => return s,
                };
                $(let $rn: &[$rt] = match batch.read::<$rt>() { Ok(c) => c, Err(s) => return s };)*
                $(let $wn: &mut [$wt] = match batch.write::<$wt>() { Ok(c) => c, Err(s) => return s };)*
                let mut body = |$ctx: &mut $crate::guest::Ctx<'_, '_>, $me: Row<'_>| -> Result<(), $crate::guest::Full> {
                    $body
                    #[allow(unreachable_code)]
                    Ok(())
                };
                let mut status = $crate::layout::status::OK;
                for i in 0..batch.n() {
                    let row = Row { $($rn: &$rn[i],)* $($wn: &mut $wn[i],)* };
                    let mut ctx = batch.ctx(i);
                    if body(&mut ctx, row).is_err() {
                        status = $crate::layout::status::COMMANDS_FULL;
                        break;
                    }
                }
                batch.finish();
                status
            }
        }
    };
}

/// The script's exports: `pe_abi_version`, `pe_manifest` and `pe_run` over the listed components and systems.
#[macro_export]
macro_rules! script {
    (components: [$($c:ty),* $(,)?], systems: [$($s:ident),* $(,)?] $(,)?) => {
        const __PE_SYSTEMS: &[$crate::guest::System] = &[$($s::SYSTEM),*];

        #[unsafe(no_mangle)]
        pub extern "C" fn pe_abi_version() -> i32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn pe_manifest() -> i64 {
            $crate::guest::manifest(&[$($crate::component::info::<$c>()),*], __PE_SYSTEMS)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn pe_run(system: i32, batch: i32) -> i32 {
            $crate::guest::run(__PE_SYSTEMS, system, batch)
        }
    };
}
