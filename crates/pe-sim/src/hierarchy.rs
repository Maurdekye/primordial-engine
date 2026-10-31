//! The object hierarchy (design 16.2): every object has a parent (or none) and an ordered list of
//! children.
//!
//! It is stored as the built-in [`Node`] component, an intrusive doubly linked list of siblings with
//! fixed-size rows, so it snapshots and hashes like any other column. Every live object has a `Node`
//! row; only this module writes them.
//!
//! Traversal order is a function of the state alone: roots in id order, then depth-first with children
//! in list order.

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::{Error, ObjectId};

use crate::registry::{Component, ComponentId, FieldDesc, FieldKind};
use crate::store::SimStore;

/// Hierarchy links. [`ObjectId::NONE`] means "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Node {
    pub parent: ObjectId,
    pub first_child: ObjectId,
    pub last_child: ObjectId,
    pub prev_sibling: ObjectId,
    pub next_sibling: ObjectId,
}

impl Node {
    pub const ROOT: Node = Node {
        parent: ObjectId::NONE,
        first_child: ObjectId::NONE,
        last_child: ObjectId::NONE,
        prev_sibling: ObjectId::NONE,
        next_sibling: ObjectId::NONE,
    };
}

impl Component for Node {
    const ID: ComponentId = ComponentId::NODE;
    const NAME: &'static str = "Node";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("parent", FieldKind::ObjectId, offset_of!(Node, parent), 1),
            FieldDesc::new("first_child", FieldKind::ObjectId, offset_of!(Node, first_child), 1),
            FieldDesc::new("last_child", FieldKind::ObjectId, offset_of!(Node, last_child), 1),
            FieldDesc::new("prev_sibling", FieldKind::ObjectId, offset_of!(Node, prev_sibling), 1),
            FieldDesc::new("next_sibling", FieldKind::ObjectId, offset_of!(Node, next_sibling), 1),
        ]
    }
}

/// Iterates an object's children in order.
pub struct Children<'a> {
    store: &'a SimStore,
    next: ObjectId,
}

impl Iterator for Children<'_> {
    type Item = ObjectId;
    fn next(&mut self) -> Option<ObjectId> {
        let id = self.next.some()?;
        self.next = self.store.node(id).next_sibling;
        Some(id)
    }
}

impl SimStore {
    /// The hierarchy links of a live object (`Node::ROOT` for an unknown id).
    pub fn node(&self, id: ObjectId) -> Node {
        self.get::<Node>(id).copied().unwrap_or(Node::ROOT)
    }

    fn set_node(&mut self, id: ObjectId, n: Node) {
        self.put_row(ComponentId::NODE, id, bytemuck::bytes_of(&n));
    }

    pub fn parent(&self, id: ObjectId) -> Option<ObjectId> {
        self.node(id).parent.some()
    }

    pub fn children(&self, id: ObjectId) -> Children<'_> {
        Children { store: self, next: self.node(id).first_child }
    }

    /// Roots (objects without a parent), in id order.
    pub fn roots(&self) -> impl Iterator<Item = ObjectId> + '_ {
        let (ids, nodes) = self.column::<Node>();
        ids.iter().zip(nodes).filter(|(_, n)| n.parent.is_none()).map(|(&id, _)| id)
    }

    /// Whether `ancestor` is `id` or one of its ancestors.
    pub fn is_ancestor_or_self(&self, ancestor: ObjectId, id: ObjectId) -> bool {
        let mut at = id.some();
        while let Some(a) = at {
            if a == ancestor {
                return true;
            }
            at = self.parent(a);
        }
        false
    }

    /// Detaches `id` from its parent and siblings; it becomes a root. Its own children stay with it.
    pub(crate) fn unlink(&mut self, id: ObjectId) {
        let n = self.node(id);
        let Some(p) = n.parent.some() else { return };
        if let Some(prev) = n.prev_sibling.some() {
            let mut pn = self.node(prev);
            pn.next_sibling = n.next_sibling;
            self.set_node(prev, pn);
        }
        if let Some(next) = n.next_sibling.some() {
            let mut nn = self.node(next);
            nn.prev_sibling = n.prev_sibling;
            self.set_node(next, nn);
        }
        let mut parent = self.node(p);
        if parent.first_child == id {
            parent.first_child = n.next_sibling;
        }
        if parent.last_child == id {
            parent.last_child = n.prev_sibling;
        }
        self.set_node(p, parent);
        self.set_node(id, Node { parent: ObjectId::NONE, prev_sibling: ObjectId::NONE, next_sibling: ObjectId::NONE, ..n });
    }

    fn check_move(&self, child: ObjectId, parent: Option<ObjectId>) -> Result<(), Error> {
        if !self.exists(child) {
            return Err(Error::UnknownObject(child));
        }
        if let Some(p) = parent {
            if !self.exists(p) {
                return Err(Error::UnknownObject(p));
            }
            if self.is_ancestor_or_self(child, p) {
                return Err(Error::Cycle { child, parent: p });
            }
        }
        Ok(())
    }

    /// The objects whose frame a move of `child` changes: `child` if it has a `Transform`, else the
    /// transformed objects below it.
    fn moved_by(&self, child: ObjectId) -> Vec<ObjectId> {
        if self.has::<crate::Transform>(child) { vec![child] } else { self.frame_children(child) }
    }

    /// Moves `child` under `parent` at position `index` among its children (`None` or past the end:
    /// last), or makes it a root (`parent = None`). Refuses a move that would create a cycle.
    ///
    /// A data edit (review P21R-3): local transforms stay as they are, so world transforms follow the new
    /// frame. Refused with [`Error::ChunkUnderFrame`] if a moved object without a frame has a non-zero
    /// `chunk` and would gain one: that includes any root `maintain_transforms` has rebased (any negative
    /// coordinate, or one past 1,024 m), so such a move needs [`SimStore::set_parent_keep_world`] (review
    /// P21S-2).
    ///
    /// Cost: O(depth) for the frame lookups and the cycle check, plus the objects moved (review P21S-4).
    /// The cycle check walks up from `parent`, so a loader that builds a long chain should link it from
    /// the bottom up, each parent still a root when its child is attached: linked top-down, every link
    /// walks the whole chain above it and the build is quadratic. A plain insert of a `Transform` onto
    /// an object without a frame also visits that object's untransformed subtree.
    pub fn set_parent(&mut self, child: ObjectId, parent: Option<ObjectId>, index: Option<usize>) -> Result<(), Error> {
        self.check_move(child, parent)?;
        let new_frame = parent.and_then(|p| self.frame_or_self(p));
        if new_frame.is_some() && self.frame_of(child).is_none() {
            for o in self.moved_by(child) {
                if self.get::<crate::Transform>(o).unwrap().chunk != [0; 3] {
                    return Err(Error::ChunkUnderFrame { object: o });
                }
            }
        }
        self.relink(child, parent, index);
        Ok(())
    }

    /// [`SimStore::set_parent`], keeping world transforms (review P21-6): the moved transforms are rewritten
    /// for their new frame, with the root chunk recomputed exactly. A move whose rewritten transforms would
    /// break the limits of [`crate::transform`] is refused, and nothing changes. Linear in the objects moved.
    pub fn set_parent_keep_world(&mut self, child: ObjectId, parent: Option<ObjectId>, index: Option<usize>) -> Result<(), Error> {
        self.check_move(child, parent)?;
        // Only a change of frame moves world transforms; the same frame composes to the same result.
        let (old_frame, new_frame) = (self.frame_of(child), parent.and_then(|p| self.frame_or_self(p)));
        let rewritten = if new_frame == old_frame {
            Vec::new()
        } else {
            self.relocalize(&self.moved_by(child), old_frame, new_frame)?
        };
        self.relink(child, parent, index);
        for (o, t) in rewritten {
            self.put_row(ComponentId::TRANSFORM, o, bytemuck::bytes_of(&t));
        }
        Ok(())
    }

    fn relink(&mut self, child: ObjectId, parent: Option<ObjectId>, index: Option<usize>) {
        self.unlink(child);
        let Some(p) = parent else { return };
        // The sibling to insert before, if any.
        let before = index.and_then(|i| self.children(p).nth(i));
        let mut cn = self.node(child);
        cn.parent = p;
        let mut pn = self.node(p);
        match before {
            Some(b) => {
                let mut bn = self.node(b);
                cn.prev_sibling = bn.prev_sibling;
                cn.next_sibling = b;
                match bn.prev_sibling.some() {
                    Some(prev) => {
                        let mut prevn = self.node(prev);
                        prevn.next_sibling = child;
                        self.set_node(prev, prevn);
                    }
                    None => pn.first_child = child,
                }
                bn.prev_sibling = child;
                self.set_node(b, bn);
            }
            None => {
                cn.prev_sibling = pn.last_child;
                cn.next_sibling = ObjectId::NONE;
                match pn.last_child.some() {
                    Some(last) => {
                        let mut ln = self.node(last);
                        ln.next_sibling = child;
                        self.set_node(last, ln);
                    }
                    None => pn.first_child = child,
                }
                pn.last_child = child;
            }
        }
        self.set_node(p, pn);
        self.set_node(child, cn);
    }

    /// `id` and all its descendants, depth-first, children in order.
    pub fn subtree(&self, id: ObjectId) -> Vec<ObjectId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            out.push(x);
            let kids: Vec<ObjectId> = self.children(x).collect();
            stack.extend(kids.into_iter().rev());
        }
        out
    }

    /// Every object depth-first: roots in id order, children in list order.
    pub fn depth_first(&self) -> Vec<ObjectId> {
        let roots: Vec<ObjectId> = self.roots().collect();
        let mut out = Vec::with_capacity(self.objects().len());
        for r in roots {
            out.extend(self.subtree(r));
        }
        out
    }
}
