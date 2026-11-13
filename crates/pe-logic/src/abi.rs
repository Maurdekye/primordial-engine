//! The guest ABI, v0 (docs/spec/logic.md 3.1 and 3.4): what a script module may import and must export.

use wasmparser::ValType::{self, F32, I32, I64};

/// What `pe_abi_version()` returns for this ABI.
pub const ABI_VERSION: i32 = 1;

/// The import module every host function lives in.
pub const IMPORT_MODULE: &str = "pe";

/// One function signature: parameters, then results.
pub struct Sig {
    pub name: &'static str,
    pub params: &'static [ValType],
    pub results: &'static [ValType],
}

/// The host functions of ABI v0 (3.4). Pointers are offsets into the module's own memory.
pub const IMPORTS: &[Sig] = &[
    // get(component, object_id_ptr, dst) -> rows copied (0 or 1): one row of a component declared in `reads_other`.
    Sig { name: "get", params: &[I32, I32, I32], results: &[I32] },
    // query_near(component, radius, dst, cap) -> count: objects near the bound one, nearest first, then by id.
    Sig { name: "query_near", params: &[I32, F32, I32, I32], results: &[I32] },
    // rand(object_id_ptr, stream) -> 64 random bits from the domain's counter-based stream for (frame, system, object,
    // stream). The guest names the object; the host keys the stream.
    Sig { name: "rand", params: &[I32, I32], results: &[I64] },
    // log(ptr, len): a diagnostic line for the author's tools; never affects state.
    Sig { name: "log", params: &[I32, I32], results: &[] },
];

/// The functions a module must export, besides its memory, named `memory`.
pub const EXPORTS: &[Sig] = &[
    Sig { name: "pe_abi_version", params: &[], results: &[I32] },
    // A pointer (low 32 bits) and length (high 32 bits) of the manifest bytes (3.2).
    Sig { name: "pe_manifest", params: &[], results: &[I64] },
    // pe_run(system, batch) -> status: one system over one batch; 0 is ok.
    Sig { name: "pe_run", params: &[I32, I32], results: &[I32] },
];

/// The export the module's memory must have.
pub const MEMORY_EXPORT: &str = "memory";

pub fn import(name: &str) -> Option<&'static Sig> {
    IMPORTS.iter().find(|s| s.name == name)
}

pub fn export(name: &str) -> Option<&'static Sig> {
    EXPORTS.iter().find(|s| s.name == name)
}

impl Sig {
    pub fn matches(&self, t: &wasmparser::FuncType) -> bool {
        t.params() == self.params && t.results() == self.results
    }

    pub fn describe(&self) -> String {
        format!("{}({:?}) -> {:?}", self.name, self.params, self.results)
    }
}
