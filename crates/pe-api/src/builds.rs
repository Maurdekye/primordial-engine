//! The build port (api.md 7.1; agreed with @vdw-physics m30a7e05b2344 and @engine-architect me97cb2e93a60): pe-api's
//! handle to the machine's one build service, which pe-app implements (its `Builder`) over pe-logic. pe-api never
//! links the compiler or the runtime.
//!
//! - One build at a time per machine, on the service's thread: the replica's attesting builds first, then API builds,
//!   first in, first out within each. A queued API build whose deadline passed leaves the queue unbuilt; a started
//!   one finishes into the cache, so a retry answers at once.
//! - One cache by V (the archive's hash), shared by `code_check`, the `code` op and the build thread.
//! - Every `build` here is an author's build: the service applies the author's margins (15 s, 512 MiB) and refuses
//!   with [`BuildRefused::Margin`], which names the margin, never `compile`.

use std::time::Instant;

use pe_core::Hash;
use pe_seams::world_api::{Diagnostic, DiagnosticKind, Diagnostics, Severity};
use pe_sim::ComponentDesc;

/// The machine's build service, as pe-api calls it.
pub trait Builds: Send + Sync {
    /// Build and validate a source archive (logic.md 4.1a) as its author's machine. Blocking, on an API worker
    /// thread, never the simulation thread; `deadline` bounds the wait in the queue.
    fn build(&self, archive: &[u8], deadline: Instant) -> Result<Built, BuildRefused>;
}

/// A build that passed.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// The metered bytes' hash, `Script::module_hash()`: exactly `CommitScript.author_module`.
    pub module: Hash,
    pub toolchain: Hash,
    /// As `CommitScript` carries it.
    pub sdk: Hash,
    /// The component types it declares, under V's ids.
    pub components: Vec<ComponentDesc>,
    pub systems: Vec<String>,
    pub warnings: Vec<Diagnostic>,
    /// The build itself at the pinned job count (logic.md 6.2b), never the time in the queue.
    pub build_ms: u32,
    /// `None` until pe-logic measures a build's peak (logic.md 6.2a item 5); no memory margin applies meanwhile.
    pub peak_bytes: Option<u64>,
}

/// One of the author's margins (rulings Q4 (b) and M1's memory cap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Margin {
    /// Over 15 s; `measured` in ms.
    Time,
    /// Over 512 MiB; `measured` in bytes.
    Memory,
}

/// A build that didn't pass, or didn't run.
#[derive(Clone, Debug, PartialEq)]
pub enum BuildRefused {
    /// rustc's diagnostics, or pe-logic's kinds. From pe-logic's `BuildError::Rustc` (@iter-bench m2704bc958fb8),
    /// one `Diagnostic` per message: `level` `error` to `Error`, `warning` to `Warning`, `note`/`help` to `Note`;
    /// `code` as is; the first primary span's `file` (crate-relative, `src/lib.rs`), `line` and `column`. An internal
    /// compiler error, or any build that fails for a reason of this machine's (`Local`), is `Unavailable`, never
    /// the author's error.
    Failed(Vec<Diagnostic>),
    /// Built, but past an author's margin.
    Margin { which: Margin, measured: u64 },
    /// The deadline passed in the queue.
    Timeout,
    /// The service can't build here now.
    Unavailable(String),
}

/// The M1 source form, one file, as an archive (logic.md 4.1a): `src/lib.rs`.
pub fn archive_of(source: &str) -> Result<Vec<u8>, String> {
    pe_world::script::pack_source(&[("src/lib.rs", source.as_bytes())]).map_err(|e| format!("{e:?}"))
}

/// A margin refusal as one diagnostic that names it.
pub fn margin_diagnostic(which: Margin, measured: u64) -> Diagnostic {
    let message = match which {
        Margin::Time => format!(
            "The build took {:.1} s, over the author's 15 s margin: every machine must build it too. Make the \
             script smaller or simpler.",
            measured as f64 / 1000.0
        ),
        Margin::Memory => format!(
            "The build used {} MiB, over the author's 512 MiB margin: every machine must build it too. Make the \
             script smaller or simpler.",
            measured >> 20
        ),
    };
    Diagnostic {
        kind: DiagnosticKind::Margin,
        severity: Severity::Error,
        message,
        file: None,
        line: None,
        column: None,
        code: None,
    }
}

/// A passed build as `code_check` answers it.
pub fn diagnostics_of(b: &Built) -> Diagnostics {
    Diagnostics {
        ok: true,
        diagnostics: b.warnings.clone(),
        module: Some(b.module.to_string()),
        systems: b.systems.clone(),
        // The version's tag is the engine's bookkeeping, never a component an agent writes (api.md 4.3).
        components: b
            .components
            .iter()
            .map(|d| d.name().to_string())
            .filter(|n| n != pe_world::script::TAG_NAME)
            .collect(),
        build_ms: Some(b.build_ms),
    }
}

/// A failed build as `code_check` answers it.
pub fn refusal_diagnostics(diagnostics: Vec<Diagnostic>) -> Diagnostics {
    Diagnostics { ok: false, diagnostics, module: None, systems: vec![], components: vec![], build_ms: None }
}
