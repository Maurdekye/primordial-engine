//! How the API names component types (api.md 4.3; ruling, after @fable-reviewer).
//!
//! pe-world's registry keeps every script version's components, and every version has its own tag, `pe.tag`. The
//! API shows only what runs:
//! - **Visible:** the engine's components (ids below `SCRIPT_ID_FLOOR`, `Node` left out) and the components of each
//!   script's active version (`ModuleRow.components`), tags left out. A pending, voided or replaced version's
//!   components never resolve and are never listed; a tag is reached only through `attach`/`detach`.
//! - **A call that names an object** resolves a bare name among the engine's components and those of the scripts
//!   attached to that object (whose active tag it carries). Two matches are refused as ambiguous, with the
//!   qualified forms; an engine name a script attached there also declares is ambiguous too, so a later engine
//!   component can never silently rebind a saved name. No match, where some script declares the name, is refused
//!   with "attach the script first".
//! - **A call with no object** (a world query's `has`, `schema`): a read takes every match.
//! - **Qualified forms** are always accepted: `Name@a:<hex>`, any unique prefix of at least [`MIN_PREFIX`] hex
//!   digits, and `Name@engine`. Responses print the full form (all 64 digits) wherever a bare name wouldn't single
//!   the component out on that object.

use std::collections::BTreeMap;

use pe_sim::ComponentId;
use pe_world::WorldSnapshot;
use pe_world::script::{AssetId, SCRIPT_ID_FLOOR, TAG_NAME};

use crate::resolve::asset_text;

/// The qualifier of an engine component's always-accepted form, `Transform@engine`.
pub const ENGINE: &str = "engine";

/// The fewest hex digits a script qualifier's prefix may have.
pub const MIN_PREFIX: usize = 4;

/// Why a name names no single component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    /// Nothing visible has the name.
    Unknown,
    /// More than one component, by their full forms.
    Ambiguous(Vec<String>),
    /// Only scripts not attached to the object declare it, by their full forms.
    NotAttached(Vec<String>),
    /// A script qualifier's prefix that is too short, or matches more than one script (those, in full).
    Prefix(Vec<String>),
}

impl NameError {
    /// The refusal's text for `name`, for a call that `on_object` names an object or not.
    pub fn message(&self, name: &str) -> String {
        match self {
            NameError::Unknown => format!("There is no component {name:?}; schema lists them."),
            NameError::Ambiguous(all) => {
                format!("{name:?} names more than one component here; name one: {}.", all.join(", "))
            }
            NameError::NotAttached(all) => format!(
                "No script attached to the object declares {name:?}; attach the script first ({}).",
                all.join(", ")
            ),
            NameError::Prefix(all) if all.is_empty() => {
                format!("{name:?}: a script qualifier needs at least {MIN_PREFIX} hex digits after \"a:\".")
            }
            NameError::Prefix(all) => {
                format!("{name:?}'s script qualifier matches {}; give more digits.", all.join(", "))
            }
        }
    }
}

/// A name as written: bare, or with its qualifier.
enum Qualifier<'s> {
    None,
    Engine,
    Script(&'s str),
}

fn split(name: &str) -> (&str, Qualifier<'_>) {
    match name.rsplit_once('@') {
        Some((bare, ENGINE)) => (bare, Qualifier::Engine),
        Some((bare, q)) if q.starts_with("a:") => (bare, Qualifier::Script(&q[2..])),
        _ => (name, Qualifier::None),
    }
}

/// One snapshot's visible component types and their names. Owned, so a view can keep it.
#[derive(Clone, Debug, Default)]
pub struct Names {
    /// Every visible component's bare name.
    name_of: BTreeMap<ComponentId, String>,
    /// Visible components by bare name, in registry order.
    by_name: BTreeMap<String, Vec<ComponentId>>,
    /// Each active version's components, tags left out: their script.
    script_of: BTreeMap<ComponentId, AssetId>,
    /// Each active version's tag, by script.
    tags: BTreeMap<AssetId, ComponentId>,
}

impl Names {
    pub fn new(snap: &WorldSnapshot) -> Names {
        let mut n = Names::default();
        let mut active: BTreeMap<ComponentId, AssetId> = BTreeMap::new();
        for (a, m) in snap.modules() {
            for c in &m.components {
                active.insert(*c, *a);
            }
        }
        for d in snap.schema() {
            let id = d.id();
            let visible = if id.0 < SCRIPT_ID_FLOOR {
                id != ComponentId::NODE
            } else if let Some(a) = active.get(&id) {
                if d.name() == TAG_NAME {
                    n.tags.insert(*a, id);
                    false
                } else {
                    n.script_of.insert(id, *a);
                    true
                }
            } else {
                false
            };
            if visible {
                n.name_of.insert(id, d.name().to_string());
                n.by_name.entry(d.name().to_string()).or_default().push(id);
            }
        }
        n
    }

    /// Whether the API shows `c` at all.
    pub fn visible(&self, c: ComponentId) -> bool {
        self.name_of.contains_key(&c)
    }

    /// The script whose active version declares `c`; `None` for an engine component.
    pub fn script_of(&self, c: ComponentId) -> Option<AssetId> {
        self.script_of.get(&c).copied()
    }

    /// `c`'s bare name, if visible.
    pub fn bare(&self, c: ComponentId) -> Option<&str> {
        self.name_of.get(&c).map(String::as_str)
    }

    /// `c`'s full form: `Name@a:<64 hex>` for a script's, `Name@engine` for the engine's.
    pub fn full(&self, c: ComponentId) -> Option<String> {
        let bare = self.bare(c)?;
        Some(match self.script_of(c) {
            Some(a) => format!("{bare}@{}", asset_text(&a)),
            None => format!("{bare}@{ENGINE}"),
        })
    }

    /// The active tag of each script, for attach state.
    pub fn tags(&self) -> impl Iterator<Item = (&AssetId, &ComponentId)> {
        self.tags.iter()
    }

    /// The scripts attached to `obj`: those whose active version's tag it carries.
    pub fn attached(&self, snap: &WorldSnapshot, obj: pe_core::ObjectId) -> Vec<AssetId> {
        self.tags.iter().filter(|(_, t)| snap.row_tag_of(obj, **t).is_some()).map(|(a, _)| *a).collect()
    }

    /// The visible components `obj` has a row of, in registry order.
    pub fn on(&self, snap: &WorldSnapshot, obj: pe_core::ObjectId) -> Vec<ComponentId> {
        snap.components_on(obj).into_iter().filter(|c| self.visible(*c)).collect()
    }

    /// The script a qualifier's prefix names, among scripts with an active version.
    fn script(&self, hex: &str) -> Result<AssetId, NameError> {
        // Hex digits in either case; responses print lowercase.
        if hex.len() < MIN_PREFIX || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(NameError::Prefix(vec![]));
        }
        let hex = hex.to_ascii_lowercase();
        let hex = hex.as_str();
        let mut found = self.tags.keys().filter(|a| asset_text(a)[2..].starts_with(hex));
        match (found.next(), found.next()) {
            (Some(a), None) => Ok(*a),
            (None, _) => Err(NameError::Unknown),
            (Some(a), Some(b)) => {
                let mut all = vec![asset_text(a), asset_text(b)];
                all.extend(found.map(asset_text));
                Err(NameError::Prefix(all))
            }
        }
    }

    /// The one component a qualified name names, whatever is attached.
    fn qualified(&self, bare: &str, q: Qualifier<'_>) -> Result<ComponentId, NameError> {
        let ids = self.by_name.get(bare).map(Vec::as_slice).unwrap_or_default();
        match q {
            Qualifier::Engine => ids.iter().copied().find(|c| self.script_of(*c).is_none()).ok_or(NameError::Unknown),
            Qualifier::Script(hex) => {
                let a = self.script(hex)?;
                ids.iter().copied().find(|c| self.script_of(*c) == Some(a)).ok_or(NameError::Unknown)
            }
            Qualifier::None => unreachable!("bare names resolve by context"),
        }
    }

    /// The component `name` names on an object to which `attached` are attached (a call that names an object).
    pub fn resolve_on(&self, attached: &[AssetId], name: &str) -> Result<ComponentId, NameError> {
        let (bare, q) = split(name);
        if !matches!(q, Qualifier::None) {
            return self.qualified(bare, q);
        }
        let ids = self.by_name.get(bare).map(Vec::as_slice).unwrap_or_default();
        let here: Vec<ComponentId> =
            ids.iter().copied().filter(|c| self.script_of(*c).is_none_or(|a| attached.contains(&a))).collect();
        let full = |cs: &[ComponentId]| cs.iter().filter_map(|c| self.full(*c)).collect::<Vec<_>>();
        match here[..] {
            [c] => Ok(c),
            [] if ids.is_empty() => Err(NameError::Unknown),
            [] => Err(NameError::NotAttached(full(ids))),
            _ => Err(NameError::Ambiguous(full(&here))),
        }
    }

    /// Every component `name` names, for a read with no object (a world query's `has`, `schema`).
    pub fn resolve_all(&self, name: &str) -> Result<Vec<ComponentId>, NameError> {
        let (bare, q) = split(name);
        if !matches!(q, Qualifier::None) {
            return self.qualified(bare, q).map(|c| vec![c]);
        }
        match self.by_name.get(bare) {
            Some(ids) if !ids.is_empty() => Ok(ids.clone()),
            _ => Err(NameError::Unknown),
        }
    }

    /// How a response names `c` on an object to which `attached` are attached: bare where the bare name resolves to
    /// it there, its full form otherwise.
    pub fn shown_on(&self, attached: &[AssetId], c: ComponentId) -> Option<String> {
        let bare = self.bare(c)?;
        if self.resolve_on(attached, bare) == Ok(c) { Some(bare.to_string()) } else { self.full(c) }
    }
}
