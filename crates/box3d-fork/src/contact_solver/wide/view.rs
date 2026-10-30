//! Primordial Engine fork (D2): raw views of the body states and of one color's wide
//! constraints, shared by the workers of a solver stage.
//!
//! Within a graph color no two constraints share a dynamic body, so in a color stage each
//! dynamic body state is read and written by one block only. Static bodies are not in the
//! awake set (index 0) and kinematic states are only read: every write is gated on the dynamic
//! flag. In a body stage each state belongs to one block. Each wide constraint and its
//! manifolds belong to one block. The views use raw pointers so that no `&mut` to shared data
//! is formed; the serial path uses the same views over the whole range.
//!
//! SPDX-License-Identifier: MIT

use super::{ContactConstraintWide, ContactManifoldWide, WideContactConstraints};
use crate::body::BodyState;

/// Read access to body states for [`super::gather_bodies`]: a slice or a [`StatesView`].
pub trait BodyStateSource {
    fn body_state(&self, index: usize) -> BodyState;
}

impl BodyStateSource for [BodyState] {
    #[inline]
    fn body_state(&self, index: usize) -> BodyState {
        self[index]
    }
}

impl BodyStateSource for StatesView {
    #[inline]
    fn body_state(&self, index: usize) -> BodyState {
        assert!(index < self.len);
        // SAFETY: in bounds; the module docs explain why nobody writes this state meanwhile.
        unsafe { self.ptr.add(index).read() }
    }
}

/// The awake body states as seen by every worker of one solver stage.
#[derive(Clone, Copy)]
pub struct StatesView {
    pub(crate) ptr: *mut BodyState,
    pub(crate) len: usize,
}

// SAFETY: see the module docs; every stage upholds the per-block disjointness.
unsafe impl Send for StatesView {}
unsafe impl Sync for StatesView {}

impl StatesView {
    pub fn new(states: &mut [BodyState]) -> StatesView {
        StatesView {
            ptr: states.as_mut_ptr(),
            len: states.len(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The states in `range` as a slice.
    ///
    /// # Safety
    /// The states outlive the slice and no other thread accesses them while it lives.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut<'a>(&self, range: core::ops::Range<usize>) -> &'a mut [BodyState] {
        assert!(range.start <= range.end && range.end <= self.len);
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(range.start), range.len()) }
    }
}

/// One color's wide constraints as seen by the workers of a stage.
#[derive(Clone, Copy)]
pub struct WideView {
    constraints: *const ContactConstraintWide,
    manifolds: *mut ContactManifoldWide,
    starts: *const i32,
    len: usize,
}

// SAFETY: see the module docs.
unsafe impl Send for WideView {}
unsafe impl Sync for WideView {}

impl WideView {
    pub fn new(set: &mut WideContactConstraints) -> WideView {
        debug_assert!(set.is_empty() || set.manifold_starts.len() == set.len() + 1);
        WideView {
            constraints: set.constraints.as_mut_ptr(),
            manifolds: set.manifolds.as_mut_ptr(),
            starts: set.manifold_starts.as_ptr(),
            len: set.constraints.len(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Constraint `index`.
    ///
    /// # Safety
    /// The set outlives the reference and is not resized meanwhile.
    #[inline]
    pub unsafe fn constraint<'a>(&self, index: usize) -> &'a ContactConstraintWide {
        assert!(index < self.len);
        unsafe { &*self.constraints.add(index) }
    }

    /// Constraint `index`, to overwrite.
    ///
    /// # Safety
    /// The set outlives the reference, and no other thread accesses this constraint meanwhile.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn constraint_mut<'a>(&self, index: usize) -> &'a mut ContactConstraintWide {
        assert!(index < self.len);
        unsafe { &mut *(self.constraints as *mut ContactConstraintWide).add(index) }
    }

    /// The manifolds of constraint `index`.
    ///
    /// # Safety
    /// The set outlives the slice, and no other thread accesses these manifolds meanwhile.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn manifolds_mut<'a>(&self, index: usize) -> &'a mut [ContactManifoldWide] {
        assert!(index < self.len);
        unsafe {
            let start = *self.starts.add(index) as usize;
            let end = *self.starts.add(index + 1) as usize;
            core::slice::from_raw_parts_mut(self.manifolds.add(start), end - start)
        }
    }
}
