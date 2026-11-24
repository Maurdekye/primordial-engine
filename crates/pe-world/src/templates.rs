//! Template folders (world-model.md 7.1, "Copy and templates"; api.md 5.3.1): world data that never takes part in
//! play, so a template can be a ready physical object that `create` with `template` copies (@coordinator-opus, after @fable-reviewer).
//!
//! - **A template folder** is a root object whose `Name` is exactly [`FOLDER`]. Every such root counts, also when
//!   there are several (pe-api then finds no template by name, but each folder is still inert).
//! - **Inert** objects are the template folders and everything under them. They are not physical (no body, so no
//!   `BodyState`, no collisions, no gravity), and no `Grab` takes them. A copy out of a folder is live like any object
//!   and starts at rest.
//! - **One predicate**: pe-world's rule and pe-api's template lookup and schema all call [`is_folder_name`] (or
//!   [`is_folder_name_ref`], the same test on the name's blob reference), so the two can't drift.
//! - **A physics edit**: renaming a root to or from [`FOLDER`], or moving an object into or out of a folder, makes its
//!   subtree's bodies inert or live. The edit's footprint covers them (`Footprint::regrouped`, and a reparent's
//!   subtree), and an edit that would make a held object inert or live is refused `Held`.
//! - The name rule is the interim. Its intended successor is an explicit marker (a component or flag) that the same
//!   predicate reads, so the rule can be swapped in one place.

use pe_core::ObjectId;

use crate::components::{BlobRef, Name};
use crate::world::World;

/// The name of a template folder.
pub const FOLDER: &str = "Templates";

/// Whether a name blob is a template folder's name: the blob of exactly [`FOLDER`]'s UTF-8 bytes. Compared by blob
/// reference, so it never needs the name's bytes.
pub fn is_folder_name_ref(name: &BlobRef) -> bool {
    *name == BlobRef::of(FOLDER.as_bytes())
}

/// Whether `name` is a template folder's name: [`is_folder_name_ref`] of its bytes.
pub fn is_folder_name(name: &str) -> bool {
    is_folder_name_ref(&BlobRef::of(name.as_bytes()))
}

impl World {
    /// Whether `id` is a template folder: a root (no parent) named [`FOLDER`].
    pub fn is_template_folder(&self, id: ObjectId) -> bool {
        self.store.exists(id)
            && self.store.parent(id).is_none()
            && self.store.get::<Name>(id).is_some_and(|n| is_folder_name_ref(&n.text))
    }

    /// Whether `id` is inert: it exists and its root is a template folder (module doc).
    pub fn inert(&self, id: ObjectId) -> bool {
        if !self.store.exists(id) {
            return false;
        }
        let mut at = id;
        for _ in 0..=crate::world::DEPTH_LIMIT_MAX {
            match self.store.parent(at) {
                Some(p) => at = p,
                None => return self.is_template_folder(at),
            }
        }
        false
    }
}
