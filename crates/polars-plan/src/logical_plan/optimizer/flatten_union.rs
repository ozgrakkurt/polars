use polars_utils::arena::{Arena, Node};
use FullAccessIR::*;

use super::OptimizationRule;
use crate::prelude::FullAccessIR;

pub struct FlattenUnionRule {}

fn get_union_inputs(node: Node, lp_arena: &Arena<FullAccessIR>) -> Option<&[Node]> {
    match lp_arena.get(node) {
        FullAccessIR::Union { inputs, .. } => Some(inputs),
        _ => None,
    }
}

impl OptimizationRule for FlattenUnionRule {
    fn optimize_plan(
        &mut self,
        lp_arena: &mut polars_utils::arena::Arena<FullAccessIR>,
        _expr_arena: &mut polars_utils::arena::Arena<crate::prelude::AExpr>,
        node: polars_utils::arena::Node,
    ) -> Option<FullAccessIR> {
        let lp = lp_arena.get(node);

        match lp {
            Union {
                inputs,
                mut options,
            } if inputs.iter().any(|node| match lp_arena.get(*node) {
                Union { options, .. } => !options.flattened_by_opt,
                _ => false,
            }) =>
            {
                let mut new_inputs = Vec::with_capacity(inputs.len() * 2);

                for node in inputs {
                    match get_union_inputs(*node, lp_arena) {
                        Some(inp) => new_inputs.extend_from_slice(inp),
                        None => new_inputs.push(*node),
                    }
                }
                options.flattened_by_opt = true;

                Some(Union {
                    inputs: new_inputs,
                    options,
                })
            },
            _ => None,
        }
    }
}
