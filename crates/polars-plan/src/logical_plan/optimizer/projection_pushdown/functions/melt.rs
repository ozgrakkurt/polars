use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn process_melt(
    proj_pd: &mut ProjectionPushDown,
    lp: FullAccessIR,
    args: &Arc<MeltArgs>,
    input: Node,
    acc_projections: Vec<ColumnNode>,
    projections_seen: usize,
    lp_arena: &mut Arena<FullAccessIR>,
    expr_arena: &mut Arena<AExpr>,
) -> PolarsResult<FullAccessIR> {
    if args.value_vars.is_empty() {
        // restart projection pushdown
        proj_pd.no_pushdown_restart_opt(lp, acc_projections, projections_seen, lp_arena, expr_arena)
    } else {
        let (mut acc_projections, mut local_projections, mut projected_names) =
            split_acc_projections(
                acc_projections,
                lp_arena.get(input).schema(lp_arena).as_ref(),
                expr_arena,
                false,
            );

        if !local_projections.is_empty() {
            local_projections.extend_from_slice(&acc_projections);
        }

        // make sure that the requested columns are projected
        args.id_vars.iter().for_each(|name| {
            add_str_to_accumulated(name, &mut acc_projections, &mut projected_names, expr_arena)
        });
        args.value_vars.iter().for_each(|name| {
            add_str_to_accumulated(name, &mut acc_projections, &mut projected_names, expr_arena)
        });

        proj_pd.pushdown_and_assign(
            input,
            acc_projections,
            projected_names,
            projections_seen,
            lp_arena,
            expr_arena,
        )?;

        // re-make melt node so that the schema is updated
        let lp = FullAccessIRBuilder::new(input, expr_arena, lp_arena)
            .melt(args.clone())
            .build();

        if local_projections.is_empty() {
            Ok(lp)
        } else {
            Ok(FullAccessIRBuilder::from_lp(lp, expr_arena, lp_arena)
                .project_simple_nodes(local_projections)
                .unwrap()
                .build())
        }
    }
}
