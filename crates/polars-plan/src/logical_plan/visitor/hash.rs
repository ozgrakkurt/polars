use std::hash::{Hash, Hasher};
use std::sync::Arc;

use polars_utils::arena::Arena;

use super::*;
use crate::logical_plan::{AExpr, FullAccessIR};
use crate::prelude::aexpr::traverse_and_hash_aexpr;
use crate::prelude::ExprIR;

impl FullAccessIRNode {
    pub(crate) fn hashable_and_cmp<'a>(&'a self, expr_arena: &'a Arena<AExpr>) -> HashableEqLP<'a> {
        HashableEqLP {
            node: self,
            expr_arena,
            ignore_cache: false,
        }
    }
}

pub(crate) struct HashableEqLP<'a> {
    node: &'a FullAccessIRNode,
    expr_arena: &'a Arena<AExpr>,
    ignore_cache: bool,
}

impl HashableEqLP<'_> {
    /// When encountering a Cache node, ignore it and take the input.
    #[cfg(feature = "cse")]
    pub(crate) fn ignore_caches(mut self) -> Self {
        self.ignore_cache = true;
        self
    }
}

fn hash_option_expr<H: Hasher>(expr: &Option<ExprIR>, expr_arena: &Arena<AExpr>, state: &mut H) {
    if let Some(e) = expr {
        e.traverse_and_hash(expr_arena, state)
    }
}

fn hash_exprs<H: Hasher>(exprs: &[ExprIR], expr_arena: &Arena<AExpr>, state: &mut H) {
    for e in exprs {
        e.traverse_and_hash(expr_arena, state);
    }
}

impl Hash for HashableEqLP<'_> {
    // This hashes the variant, not the whole plan
    fn hash<H: Hasher>(&self, state: &mut H) {
        let alp = self.node.to_alp();
        std::mem::discriminant(alp).hash(state);
        match alp {
            #[cfg(feature = "python")]
            FullAccessIR::PythonScan { .. } => {},
            FullAccessIR::Slice {
                offset,
                len,
                input: _,
            } => {
                len.hash(state);
                offset.hash(state);
            },
            FullAccessIR::Filter {
                input: _,
                predicate,
            } => {
                predicate.traverse_and_hash(self.expr_arena, state);
            },
            FullAccessIR::Scan {
                paths,
                file_info: _,
                predicate,
                output_schema: _,
                scan_type,
                file_options,
            } => {
                // We don't have to traverse the schema, hive partitions etc. as they are derivative from the paths.
                scan_type.hash(state);
                paths.hash(state);
                hash_option_expr(predicate, self.expr_arena, state);
                file_options.hash(state);
            },
            FullAccessIR::DataFrameScan {
                df,
                schema: _,
                output_schema: _,
                projection,
                selection,
            } => {
                (Arc::as_ptr(df) as usize).hash(state);
                projection.hash(state);
                hash_option_expr(selection, self.expr_arena, state);
            },
            FullAccessIR::SimpleProjection {
                columns,
                duplicate_check,
                input: _,
            } => {
                columns.hash(state);
                duplicate_check.hash(state);
            },
            FullAccessIR::Select {
                input: _,
                expr,
                schema: _,
                options,
            } => {
                hash_exprs(expr.default_exprs(), self.expr_arena, state);
                options.hash(state);
            },
            FullAccessIR::Sort {
                input: _,
                by_column,
                args,
            } => {
                hash_exprs(by_column, self.expr_arena, state);
                args.hash(state);
            },
            FullAccessIR::GroupBy {
                input: _,
                keys,
                aggs,
                schema: _,
                apply,
                maintain_order,
                options,
            } => {
                hash_exprs(keys, self.expr_arena, state);
                hash_exprs(aggs, self.expr_arena, state);
                apply.is_none().hash(state);
                maintain_order.hash(state);
                options.hash(state);
            },
            FullAccessIR::Join {
                input_left: _,
                input_right: _,
                schema: _,
                left_on,
                right_on,
                options,
            } => {
                hash_exprs(left_on, self.expr_arena, state);
                hash_exprs(right_on, self.expr_arena, state);
                options.hash(state);
            },
            FullAccessIR::HStack {
                input: _,
                exprs,
                schema: _,
                options,
            } => {
                hash_exprs(exprs.default_exprs(), self.expr_arena, state);
                options.hash(state);
            },
            FullAccessIR::Distinct { input: _, options } => {
                options.hash(state);
            },
            FullAccessIR::MapFunction { input: _, function } => {
                function.hash(state);
            },
            FullAccessIR::Union { inputs: _, options } => options.hash(state),
            FullAccessIR::HConcat {
                inputs: _,
                schema: _,
                options,
            } => {
                options.hash(state);
            },
            FullAccessIR::ExtContext {
                input: _,
                contexts,
                schema: _,
            } => {
                for node in contexts {
                    traverse_and_hash_aexpr(*node, self.expr_arena, state);
                }
            },
            FullAccessIR::Sink { input: _, payload } => {
                payload.hash(state);
            },
            FullAccessIR::Cache {
                input: _,
                id,
                cache_hits,
            } => {
                id.hash(state);
                cache_hits.hash(state);
            },
            FullAccessIR::Invalid => unreachable!(),
        }
    }
}

fn expr_irs_eq(l: &[ExprIR], r: &[ExprIR], expr_arena: &Arena<AExpr>) -> bool {
    l.len() == r.len() && l.iter().zip(r).all(|(l, r)| expr_ir_eq(l, r, expr_arena))
}

fn expr_ir_eq(l: &ExprIR, r: &ExprIR, expr_arena: &Arena<AExpr>) -> bool {
    l.get_alias() == r.get_alias() && {
        let expr_arena = expr_arena as *const _ as *mut _;
        unsafe {
            let l = AexprNode::from_raw(l.node(), expr_arena);
            let r = AexprNode::from_raw(r.node(), expr_arena);
            l == r
        }
    }
}

fn opt_expr_ir_eq(l: &Option<ExprIR>, r: &Option<ExprIR>, expr_arena: &Arena<AExpr>) -> bool {
    match (l, r) {
        (None, None) => true,
        (Some(l), Some(r)) => expr_ir_eq(l, r, expr_arena),
        _ => false,
    }
}

impl HashableEqLP<'_> {
    fn is_equal(&self, other: &Self) -> bool {
        let alp_l = self.node.to_alp();
        let alp_r = other.node.to_alp();
        if std::mem::discriminant(alp_l) != std::mem::discriminant(alp_r) {
            return false;
        }
        match (alp_l, alp_r) {
            (
                FullAccessIR::Slice {
                    input: _,
                    offset: ol,
                    len: ll,
                },
                FullAccessIR::Slice {
                    input: _,
                    offset: or,
                    len: lr,
                },
            ) => ol == or && ll == lr,
            (
                FullAccessIR::Filter {
                    input: _,
                    predicate: l,
                },
                FullAccessIR::Filter {
                    input: _,
                    predicate: r,
                },
            ) => expr_ir_eq(l, r, self.expr_arena),
            (
                FullAccessIR::Scan {
                    paths: pl,
                    file_info: _,
                    predicate: pred_l,
                    output_schema: _,
                    scan_type: stl,
                    file_options: ol,
                },
                FullAccessIR::Scan {
                    paths: pr,
                    file_info: _,
                    predicate: pred_r,
                    output_schema: _,
                    scan_type: str,
                    file_options: or,
                },
            ) => {
                pl == pr
                    && stl == str
                    && ol == or
                    && opt_expr_ir_eq(pred_l, pred_r, self.expr_arena)
            },
            (
                FullAccessIR::DataFrameScan {
                    df: dfl,
                    schema: _,
                    output_schema: _,
                    projection: pl,
                    selection: sl,
                },
                FullAccessIR::DataFrameScan {
                    df: dfr,
                    schema: _,
                    output_schema: _,
                    projection: pr,
                    selection: sr,
                },
            ) => {
                Arc::as_ptr(dfl) == Arc::as_ptr(dfr)
                    && pl == pr
                    && opt_expr_ir_eq(sl, sr, self.expr_arena)
            },
            (
                FullAccessIR::SimpleProjection {
                    input: _,
                    columns: cl,
                    duplicate_check: dl,
                },
                FullAccessIR::SimpleProjection {
                    input: _,
                    columns: cr,
                    duplicate_check: dr,
                },
            ) => dl == dr && cl == cr,
            (
                FullAccessIR::Select {
                    input: _,
                    expr: el,
                    options: ol,
                    schema: _,
                },
                FullAccessIR::Select {
                    input: _,
                    expr: er,
                    options: or,
                    schema: _,
                },
            ) => ol == or && expr_irs_eq(el.default_exprs(), er.default_exprs(), self.expr_arena),
            (
                FullAccessIR::Sort {
                    input: _,
                    by_column: cl,
                    args: al,
                },
                FullAccessIR::Sort {
                    input: _,
                    by_column: cr,
                    args: ar,
                },
            ) => al == ar && expr_irs_eq(cl, cr, self.expr_arena),
            (
                FullAccessIR::GroupBy {
                    input: _,
                    keys: keys_l,
                    aggs: aggs_l,
                    schema: _,
                    apply: apply_l,
                    maintain_order: maintain_l,
                    options: ol,
                },
                FullAccessIR::GroupBy {
                    input: _,
                    keys: keys_r,
                    aggs: aggs_r,
                    schema: _,
                    apply: apply_r,
                    maintain_order: maintain_r,
                    options: or,
                },
            ) => {
                apply_l.is_none()
                    && apply_r.is_none()
                    && ol == or
                    && maintain_l == maintain_r
                    && expr_irs_eq(keys_l, keys_r, self.expr_arena)
                    && expr_irs_eq(aggs_l, aggs_r, self.expr_arena)
            },
            (
                FullAccessIR::Join {
                    input_left: _,
                    input_right: _,
                    schema: _,
                    left_on: ll,
                    right_on: rl,
                    options: ol,
                },
                FullAccessIR::Join {
                    input_left: _,
                    input_right: _,
                    schema: _,
                    left_on: lr,
                    right_on: rr,
                    options: or,
                },
            ) => {
                ol == or
                    && expr_irs_eq(ll, lr, self.expr_arena)
                    && expr_irs_eq(rl, rr, self.expr_arena)
            },
            (
                FullAccessIR::HStack {
                    input: _,
                    exprs: el,
                    schema: _,
                    options: ol,
                },
                FullAccessIR::HStack {
                    input: _,
                    exprs: er,
                    schema: _,
                    options: or,
                },
            ) => ol == or && expr_irs_eq(el.default_exprs(), er.default_exprs(), self.expr_arena),
            (
                FullAccessIR::Distinct {
                    input: _,
                    options: ol,
                },
                FullAccessIR::Distinct {
                    input: _,
                    options: or,
                },
            ) => ol == or,
            (
                FullAccessIR::MapFunction {
                    input: _,
                    function: l,
                },
                FullAccessIR::MapFunction {
                    input: _,
                    function: r,
                },
            ) => l == r,
            (
                FullAccessIR::Union {
                    inputs: _,
                    options: l,
                },
                FullAccessIR::Union {
                    inputs: _,
                    options: r,
                },
            ) => l == r,
            (
                FullAccessIR::HConcat {
                    inputs: _,
                    schema: _,
                    options: l,
                },
                FullAccessIR::HConcat {
                    inputs: _,
                    schema: _,
                    options: r,
                },
            ) => l == r,
            (
                FullAccessIR::ExtContext {
                    input: _,
                    contexts: l,
                    schema: _,
                },
                FullAccessIR::ExtContext {
                    input: _,
                    contexts: r,
                    schema: _,
                },
            ) => {
                l.len() == r.len()
                    && l.iter().zip(r.iter()).all(|(l, r)| {
                        let expr_arena = self.expr_arena as *const _ as *mut _;
                        unsafe {
                            let l = AexprNode::from_raw(*l, expr_arena);
                            let r = AexprNode::from_raw(*r, expr_arena);
                            l == r
                        }
                    })
            },
            _ => false,
        }
    }
}

impl PartialEq for HashableEqLP<'_> {
    fn eq(&self, other: &Self) -> bool {
        let mut scratch_1 = vec![];
        let mut scratch_2 = vec![];

        scratch_1.push(self.node.node());
        scratch_2.push(other.node.node());

        loop {
            match (scratch_1.pop(), scratch_2.pop()) {
                (Some(l), Some(r)) => {
                    // SAFETY: we can pass a *mut pointer
                    // the equality operation will not access mutable
                    let l = unsafe { FullAccessIRNode::from_raw(l, self.node.get_arena_raw()) };
                    let r = unsafe { FullAccessIRNode::from_raw(r, self.node.get_arena_raw()) };
                    let l_alp = l.to_alp();
                    let r_alp = r.to_alp();

                    if self.ignore_cache {
                        match (l_alp, r_alp) {
                            (
                                FullAccessIR::Cache { input: l, .. },
                                FullAccessIR::Cache { input: r, .. },
                            ) => {
                                scratch_1.push(*l);
                                scratch_2.push(*r);
                                continue;
                            },
                            (FullAccessIR::Cache { input: l, .. }, _) => {
                                scratch_1.push(*l);
                                scratch_2.push(r.node());
                                continue;
                            },
                            (_, FullAccessIR::Cache { input: r, .. }) => {
                                scratch_1.push(l.node());
                                scratch_2.push(*r);
                                continue;
                            },
                            _ => {},
                        }
                    }

                    if !l
                        .hashable_and_cmp(self.expr_arena)
                        .is_equal(&r.hashable_and_cmp(self.expr_arena))
                    {
                        return false;
                    }

                    l.to_alp().copy_inputs(&mut scratch_1);
                    r.to_alp().copy_inputs(&mut scratch_2);
                },
                (None, None) => return true,
                _ => return false,
            }
        }
    }
}
