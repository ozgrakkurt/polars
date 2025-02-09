use std::borrow::Cow;

use polars_core::schema::SchemaRef;
use polars_utils::unitvec;

use super::*;
use crate::prelude::*;

#[derive(Copy, Clone, Debug)]
pub struct FullAccessIRNode {
    node: Node,
    arena: *mut Arena<FullAccessIR>,
}

impl FullAccessIRNode {
    /// Don't use this directly, use [`Self::with_context`]
    ///
    /// # Safety
    /// This will keep a pointer to `arena`. The caller must ensure it stays alive.
    unsafe fn new(node: Node, arena: &mut Arena<FullAccessIR>) -> Self {
        Self { node, arena }
    }

    /// # Safety
    /// This will keep a pointer to `arena`. The caller must ensure it stays alive.
    pub(crate) unsafe fn from_raw(node: Node, arena: *mut Arena<FullAccessIR>) -> Self {
        Self { node, arena }
    }

    #[cfg(feature = "cse")]
    pub(crate) fn get_arena_raw(&self) -> *mut Arena<FullAccessIR> {
        self.arena
    }

    /// Safe interface. Take the `&mut Arena` only for the duration of `op`.
    pub fn with_context<F, T>(node: Node, arena: &mut Arena<FullAccessIR>, mut op: F) -> T
    where
        F: FnMut(FullAccessIRNode) -> T,
    {
        // SAFETY: we drop this context before arena is out of scope
        unsafe { op(Self::new(node, arena)) }
    }

    pub fn node(&self) -> Node {
        self.node
    }

    pub fn with_arena<'a, F, T>(&self, op: F) -> T
    where
        F: Fn(&'a Arena<FullAccessIR>) -> T,
    {
        let arena = unsafe { &(*self.arena) };

        op(arena)
    }

    pub fn with_arena_mut<'a, F, T>(&mut self, op: F) -> T
    where
        F: FnOnce(&'a mut Arena<FullAccessIR>) -> T,
    {
        let arena = unsafe { &mut (*self.arena) };

        op(arena)
    }

    /// Add a new `FullAccessIR` to the arena and set that node to `Self`.
    pub fn assign(&mut self, ae: FullAccessIR) {
        let node = self.with_arena_mut(|arena| arena.add(ae));
        self.node = node
    }

    pub fn replace_node(&mut self, node: Node) {
        self.node = node;
    }

    /// Replace the current `Node` with a new `FullAccessIR`.
    pub fn replace(&mut self, ae: FullAccessIR) {
        let node = self.node;
        self.with_arena_mut(|arena| arena.replace(node, ae));
    }

    pub fn to_alp(&self) -> &FullAccessIR {
        self.with_arena(|arena| arena.get(self.node))
    }

    pub fn to_alp_mut(&mut self) -> &mut FullAccessIR {
        let node = self.node;
        self.with_arena_mut(|arena| arena.get_mut(node))
    }

    pub fn schema(&self) -> Cow<SchemaRef> {
        self.with_arena(|arena| arena.get(self.node).schema(arena))
    }

    /// Take a [`Node`] and convert it an [`FullAccessIRNode`] and call
    /// `F` with `self` and the new created [`FullAccessIRNode`]
    pub fn binary<F, T>(&self, other: Node, op: F) -> T
    where
        F: FnOnce(&FullAccessIRNode, &FullAccessIRNode) -> T,
    {
        // this is safe as we remain in context
        let other = unsafe { FullAccessIRNode::from_raw(other, self.arena) };
        op(self, &other)
    }
}

impl TreeWalker for FullAccessIRNode {
    fn apply_children<'a>(
        &'a self,
        op: &mut dyn FnMut(&Self) -> PolarsResult<VisitRecursion>,
    ) -> PolarsResult<VisitRecursion> {
        let mut scratch = unitvec![];

        self.to_alp().copy_inputs(&mut scratch);
        for &node in scratch.as_slice() {
            let lp_node = FullAccessIRNode {
                node,
                arena: self.arena,
            };
            match op(&lp_node)? {
                // let the recursion continue
                VisitRecursion::Continue | VisitRecursion::Skip => {},
                // early stop
                VisitRecursion::Stop => return Ok(VisitRecursion::Stop),
            }
        }
        Ok(VisitRecursion::Continue)
    }

    fn map_children(
        mut self,
        op: &mut dyn FnMut(Self) -> PolarsResult<Self>,
    ) -> PolarsResult<Self> {
        let mut inputs = vec![];
        let mut exprs = vec![];

        let lp = self.to_alp();
        lp.copy_inputs(&mut inputs);
        lp.copy_exprs(&mut exprs);

        // rewrite the nodes
        for node in &mut inputs {
            let lp_node = FullAccessIRNode {
                node: *node,
                arena: self.arena,
            };
            *node = op(lp_node)?.node;
        }

        let lp = lp.with_exprs_and_input(exprs, inputs);
        self.with_arena_mut(move |arena| arena.replace(self.node, lp));
        Ok(self)
    }
}
