mod functions;
mod generic;
mod group_by;
mod hconcat;
mod hstack;
mod joins;
mod projection;
mod rename;
#[cfg(feature = "semi_anti_join")]
mod semi_anti_join;

use polars_core::datatypes::PlHashSet;
use polars_core::prelude::*;
use polars_io::RowIndex;
use recursive::recursive;
#[cfg(feature = "semi_anti_join")]
use semi_anti_join::process_semi_anti_join;

use crate::prelude::optimizer::projection_pushdown::generic::process_generic;
use crate::prelude::optimizer::projection_pushdown::group_by::process_group_by;
use crate::prelude::optimizer::projection_pushdown::hconcat::process_hconcat;
use crate::prelude::optimizer::projection_pushdown::hstack::process_hstack;
use crate::prelude::optimizer::projection_pushdown::joins::process_join;
use crate::prelude::optimizer::projection_pushdown::projection::process_projection;
use crate::prelude::optimizer::projection_pushdown::rename::process_rename;
use crate::prelude::*;
use crate::utils::aexpr_to_leaf_names;

fn init_vec() -> Vec<ColumnNode> {
    Vec::with_capacity(16)
}
fn init_set() -> PlHashSet<Arc<str>> {
    PlHashSet::with_capacity(32)
}

/// utility function to get names of the columns needed in projection at scan level
fn get_scan_columns(
    acc_projections: &mut Vec<ColumnNode>,
    expr_arena: &Arena<AExpr>,
    row_index: Option<&RowIndex>,
) -> Option<Arc<Vec<String>>> {
    let mut with_columns = None;
    if !acc_projections.is_empty() {
        let mut columns = Vec::with_capacity(acc_projections.len());
        for expr in acc_projections {
            let name = column_node_to_name(*expr, expr_arena);
            // we shouldn't project the row-count column, as that is generated
            // in the scan
            let push = match row_index {
                Some(rc) if name.as_ref() != rc.name.as_str() => true,
                None => true,
                _ => false,
            };
            if push {
                columns.push((*name).to_owned())
            }
        }
        with_columns = Some(Arc::new(columns));
    }
    with_columns
}

/// split in a projection vec that can be pushed down and a projection vec that should be used
/// in this node
///
/// # Returns
/// accumulated_projections, local_projections, accumulated_names
///
/// - `expands_schema`. An unnest adds more columns to a schema, so we cannot use fast path
fn split_acc_projections(
    acc_projections: Vec<ColumnNode>,
    down_schema: &Schema,
    expr_arena: &Arena<AExpr>,
    expands_schema: bool,
) -> (Vec<ColumnNode>, Vec<ColumnNode>, PlHashSet<Arc<str>>) {
    // If node above has as many columns as the projection there is nothing to pushdown.
    if !expands_schema && down_schema.len() == acc_projections.len() {
        let local_projections = acc_projections;
        (vec![], local_projections, PlHashSet::new())
    } else {
        let (acc_projections, local_projections): (Vec<_>, Vec<_>) = acc_projections
            .into_iter()
            .partition(|expr| check_input_column_node(*expr, down_schema, expr_arena));
        let mut names = init_set();
        for proj in &acc_projections {
            let name = column_node_to_name(*proj, expr_arena);
            names.insert(name);
        }
        (acc_projections, local_projections, names)
    }
}

/// utility function such that we can recurse all binary expressions in the expression tree
fn add_expr_to_accumulated(
    expr: Node,
    acc_projections: &mut Vec<ColumnNode>,
    projected_names: &mut PlHashSet<Arc<str>>,
    expr_arena: &Arena<AExpr>,
) {
    for root_node in aexpr_to_column_nodes_iter(expr, expr_arena) {
        let name = column_node_to_name(root_node, expr_arena);
        if projected_names.insert(name) {
            acc_projections.push(root_node)
        }
    }
}

fn add_str_to_accumulated(
    name: &str,
    acc_projections: &mut Vec<ColumnNode>,
    projected_names: &mut PlHashSet<Arc<str>>,
    expr_arena: &mut Arena<AExpr>,
) {
    // if empty: all columns are already projected.
    if !acc_projections.is_empty() && !projected_names.contains(name) {
        let node = expr_arena.add(AExpr::Column(ColumnName::from(name)));
        add_expr_to_accumulated(node, acc_projections, projected_names, expr_arena);
    }
}

fn update_scan_schema(
    acc_projections: &[ColumnNode],
    expr_arena: &Arena<AExpr>,
    schema: &Schema,
    sort_projections: bool,
) -> PolarsResult<Schema> {
    let mut new_schema = Schema::with_capacity(acc_projections.len());
    let mut new_cols = Vec::with_capacity(acc_projections.len());
    for node in acc_projections.iter() {
        let name = column_node_to_name(*node, expr_arena);
        let item = schema.try_get_full(&name)?;
        new_cols.push(item);
    }
    // make sure that the projections are sorted by the schema.
    if sort_projections {
        new_cols.sort_unstable_by_key(|item| item.0);
    }
    for item in new_cols {
        new_schema.with_column(item.1.clone(), item.2.clone());
    }
    Ok(new_schema)
}

pub struct ProjectionPushDown {
    pub is_count_star: bool,
}

impl ProjectionPushDown {
    pub(super) fn new() -> Self {
        Self {
            is_count_star: false,
        }
    }

    /// Projection will be done at this node, but we continue optimization
    fn no_pushdown_restart_opt(
        &mut self,
        lp: FullAccessIR,
        acc_projections: Vec<ColumnNode>,
        projections_seen: usize,
        lp_arena: &mut Arena<FullAccessIR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<FullAccessIR> {
        let inputs = lp.get_inputs();
        let exprs = lp.get_exprs();

        let new_inputs = inputs
            .iter()
            .map(|&node| {
                let alp = lp_arena.take(node);
                let alp = self.push_down(
                    alp,
                    Default::default(),
                    Default::default(),
                    projections_seen,
                    lp_arena,
                    expr_arena,
                )?;
                lp_arena.replace(node, alp);
                Ok(node)
            })
            .collect::<PolarsResult<Vec<_>>>()?;
        let lp = lp.with_exprs_and_input(exprs, new_inputs);

        let builder = FullAccessIRBuilder::from_lp(lp, expr_arena, lp_arena);
        Ok(self.finish_node_simple_projection(&acc_projections, builder))
    }

    fn finish_node_simple_projection(
        &mut self,
        local_projections: &[ColumnNode],
        builder: FullAccessIRBuilder,
    ) -> FullAccessIR {
        if !local_projections.is_empty() {
            builder
                .project_simple_nodes(local_projections.iter().map(|node| node.0))
                .unwrap()
                .build()
        } else {
            builder.build()
        }
    }

    fn finish_node(
        &mut self,
        local_projections: Vec<ExprIR>,
        builder: FullAccessIRBuilder,
    ) -> FullAccessIR {
        if !local_projections.is_empty() {
            builder
                .project(local_projections, Default::default())
                .build()
        } else {
            builder.build()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn join_push_down(
        &mut self,
        schema_left: &Schema,
        schema_right: &Schema,
        proj: ColumnNode,
        pushdown_left: &mut Vec<ColumnNode>,
        pushdown_right: &mut Vec<ColumnNode>,
        names_left: &mut PlHashSet<Arc<str>>,
        names_right: &mut PlHashSet<Arc<str>>,
        expr_arena: &Arena<AExpr>,
    ) -> (bool, bool) {
        let mut pushed_at_least_one = false;
        let mut already_projected = false;

        let name = column_node_to_name(proj, expr_arena);
        let is_in_left = names_left.contains(&name);
        let is_in_right = names_right.contains(&name);
        already_projected |= is_in_left;
        already_projected |= is_in_right;

        if check_input_column_node(proj, schema_left, expr_arena) && !is_in_left {
            names_left.insert(name.clone());
            pushdown_left.push(proj);
            pushed_at_least_one = true;
        }
        if check_input_column_node(proj, schema_right, expr_arena) && !is_in_right {
            names_right.insert(name.clone());
            pushdown_right.push(proj);
            pushed_at_least_one = true;
        }

        (pushed_at_least_one, already_projected)
    }

    /// This pushes down current node and assigns the result to this node.
    fn pushdown_and_assign(
        &mut self,
        input: Node,
        acc_projections: Vec<ColumnNode>,
        names: PlHashSet<Arc<str>>,
        projections_seen: usize,
        lp_arena: &mut Arena<FullAccessIR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<()> {
        let alp = lp_arena.take(input);
        let lp = self.push_down(
            alp,
            acc_projections,
            names,
            projections_seen,
            lp_arena,
            expr_arena,
        )?;
        lp_arena.replace(input, lp);
        Ok(())
    }

    /// This pushes down the projection that are validated
    /// that they can be done successful at the schema above
    /// The result is assigned to this node.
    ///
    /// The local projections are return and still have to be applied
    fn pushdown_and_assign_check_schema(
        &mut self,
        input: Node,
        acc_projections: Vec<ColumnNode>,
        projections_seen: usize,
        lp_arena: &mut Arena<FullAccessIR>,
        expr_arena: &mut Arena<AExpr>,
        // an unnest changes/expands the schema
        expands_schema: bool,
    ) -> PolarsResult<Vec<ColumnNode>> {
        let alp = lp_arena.take(input);
        let down_schema = alp.schema(lp_arena);

        let (acc_projections, local_projections, names) =
            split_acc_projections(acc_projections, &down_schema, expr_arena, expands_schema);

        let lp = self.push_down(
            alp,
            acc_projections,
            names,
            projections_seen,
            lp_arena,
            expr_arena,
        )?;
        lp_arena.replace(input, lp);
        Ok(local_projections)
    }

    /// Projection pushdown optimizer
    ///
    /// # Arguments
    ///
    /// * `FullAccessIR` - Arena based logical plan tree representing the query.
    /// * `acc_projections` - The projections we accumulate during tree traversal.
    /// * `names` - We keep track of the names to ensure we don't do duplicate projections.
    /// * `projections_seen` - Count the number of projection operations during tree traversal.
    /// * `lp_arena` - The local memory arena for the logical plan.
    /// * `expr_arena` - The local memory arena for the expressions.
    #[recursive]
    fn push_down(
        &mut self,
        logical_plan: FullAccessIR,
        mut acc_projections: Vec<ColumnNode>,
        mut projected_names: PlHashSet<Arc<str>>,
        projections_seen: usize,
        lp_arena: &mut Arena<FullAccessIR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<FullAccessIR> {
        use FullAccessIR::*;

        match logical_plan {
            Select { expr, input, .. } => process_projection(
                self,
                input,
                expr.exprs(),
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            SimpleProjection { columns, input, .. } => {
                let exprs = names_to_expr_irs(columns.iter_names(), expr_arena);
                process_projection(
                    self,
                    input,
                    exprs,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                )
            },
            DataFrameScan {
                df,
                schema,
                mut output_schema,
                selection,
                ..
            } => {
                let mut projection = None;
                if !acc_projections.is_empty() {
                    output_schema = Some(Arc::new(update_scan_schema(
                        &acc_projections,
                        expr_arena,
                        &schema,
                        false,
                    )?));
                    projection = get_scan_columns(&mut acc_projections, expr_arena, None);
                }
                let lp = DataFrameScan {
                    df,
                    schema,
                    output_schema,
                    projection,
                    selection,
                };
                Ok(lp)
            },
            #[cfg(feature = "python")]
            PythonScan {
                mut options,
                predicate,
            } => {
                options.with_columns = get_scan_columns(&mut acc_projections, expr_arena, None);

                options.output_schema = if options.with_columns.is_none() {
                    None
                } else {
                    Some(Arc::new(update_scan_schema(
                        &acc_projections,
                        expr_arena,
                        &options.schema,
                        true,
                    )?))
                };
                Ok(PythonScan { options, predicate })
            },
            Scan {
                paths,
                file_info,
                scan_type,
                predicate,
                mut file_options,
                mut output_schema,
            } => {
                let mut do_optimization = true;
                #[allow(irrefutable_let_patterns)]
                if let FileScan::Anonymous { ref function, .. } = scan_type {
                    do_optimization = function.allows_projection_pushdown();
                }

                if do_optimization {
                    file_options.with_columns = get_scan_columns(
                        &mut acc_projections,
                        expr_arena,
                        file_options.row_index.as_ref(),
                    );

                    output_schema = if file_options.with_columns.is_none() {
                        None
                    } else {
                        let mut schema = update_scan_schema(
                            &acc_projections,
                            expr_arena,
                            &file_info.schema,
                            scan_type.sort_projection(&file_options),
                        )?;
                        // Hive partitions are created AFTER the projection, so the output
                        // schema is incorrect. Here we ensure the columns that are projected and hive
                        // parts are added at the proper place in the schema, which is at the end.
                        if let Some(parts) = file_info.hive_parts.as_deref() {
                            let partition_schema = parts.schema();
                            for (name, _) in partition_schema.iter() {
                                if let Some(dt) = schema.shift_remove(name) {
                                    schema.with_column(name.clone(), dt);
                                }
                            }
                        }
                        Some(Arc::new(schema))
                    };
                }

                let lp = Scan {
                    paths,
                    file_info,
                    output_schema,
                    scan_type,
                    predicate,
                    file_options,
                };
                Ok(lp)
            },
            Sort {
                input,
                by_column,
                args,
            } => {
                if !acc_projections.is_empty() {
                    // Make sure that the column(s) used for the sort is projected
                    by_column.iter().for_each(|node| {
                        add_expr_to_accumulated(
                            node.node(),
                            &mut acc_projections,
                            &mut projected_names,
                            expr_arena,
                        );
                    });
                }

                self.pushdown_and_assign(
                    input,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                )?;
                Ok(Sort {
                    input,
                    by_column,
                    args,
                })
            },
            Distinct { input, options } => {
                // make sure that the set of unique columns is projected
                if !acc_projections.is_empty() {
                    if let Some(subset) = options.subset.as_ref() {
                        subset.iter().for_each(|name| {
                            add_str_to_accumulated(
                                name,
                                &mut acc_projections,
                                &mut projected_names,
                                expr_arena,
                            )
                        })
                    } else {
                        // distinct needs all columns
                        let input_schema = lp_arena.get(input).schema(lp_arena);
                        for name in input_schema.iter_names() {
                            add_str_to_accumulated(
                                name.as_str(),
                                &mut acc_projections,
                                &mut projected_names,
                                expr_arena,
                            )
                        }
                    }
                }

                self.pushdown_and_assign(
                    input,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                )?;
                Ok(Distinct { input, options })
            },
            Filter { predicate, input } => {
                if !acc_projections.is_empty() {
                    // make sure that the filter column is projected
                    add_expr_to_accumulated(
                        predicate.node(),
                        &mut acc_projections,
                        &mut projected_names,
                        expr_arena,
                    );
                };
                self.pushdown_and_assign(
                    input,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                )?;
                Ok(Filter { predicate, input })
            },
            GroupBy {
                input,
                keys,
                aggs,
                apply,
                schema,
                maintain_order,
                options,
            } => process_group_by(
                self,
                input,
                keys,
                aggs,
                apply,
                schema,
                maintain_order,
                options,
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                options,
                schema,
            } => match options.args.how {
                #[cfg(feature = "semi_anti_join")]
                JoinType::Semi | JoinType::Anti => process_semi_anti_join(
                    self,
                    input_left,
                    input_right,
                    left_on,
                    right_on,
                    options,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                ),
                _ => process_join(
                    self,
                    input_left,
                    input_right,
                    left_on,
                    right_on,
                    options,
                    acc_projections,
                    projected_names,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                    &schema,
                ),
            },
            HStack {
                input,
                exprs,
                options,
                ..
            } => process_hstack(
                self,
                input,
                exprs.exprs(),
                options,
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            ExtContext {
                input, contexts, ..
            } => {
                // local projections are ignored. These are just root nodes
                // complex expression will still be done later
                let _local_projections = self.pushdown_and_assign_check_schema(
                    input,
                    acc_projections,
                    projections_seen,
                    lp_arena,
                    expr_arena,
                    false,
                )?;

                let mut new_schema = lp_arena
                    .get(input)
                    .schema(lp_arena)
                    .as_ref()
                    .as_ref()
                    .clone();

                for node in &contexts {
                    let other_schema = lp_arena.get(*node).schema(lp_arena);
                    for fld in other_schema.iter_fields() {
                        if new_schema.get(fld.name()).is_none() {
                            new_schema.with_column(fld.name, fld.dtype);
                        }
                    }
                }

                Ok(ExtContext {
                    input,
                    contexts,
                    schema: Arc::new(new_schema),
                })
            },
            MapFunction {
                input,
                ref function,
            } => functions::process_functions(
                self,
                input,
                function,
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            HConcat {
                inputs,
                schema,
                options,
            } => process_hconcat(
                self,
                inputs,
                schema,
                options,
                acc_projections,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            lp @ Union { .. } => process_generic(
                self,
                lp,
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            // These nodes only have inputs and exprs, so we can use same logic.
            lp @ Slice { .. } | lp @ Sink { .. } => process_generic(
                self,
                lp,
                acc_projections,
                projected_names,
                projections_seen,
                lp_arena,
                expr_arena,
            ),
            Cache { .. } => {
                // projections above this cache will be accumulated and pushed down
                // later
                // the redundant projection will be cleaned in the fast projection optimization
                // phase.
                if acc_projections.is_empty() {
                    Ok(logical_plan)
                } else {
                    Ok(
                        FullAccessIRBuilder::from_lp(logical_plan, expr_arena, lp_arena)
                            .project_simple_nodes(acc_projections)
                            .unwrap()
                            .build(),
                    )
                }
            },
            Invalid => unreachable!(),
        }
    }

    pub fn optimize(
        &mut self,
        logical_plan: FullAccessIR,
        lp_arena: &mut Arena<FullAccessIR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<FullAccessIR> {
        let acc_projections = init_vec();
        let names = init_set();
        self.push_down(
            logical_plan,
            acc_projections,
            names,
            0,
            lp_arena,
            expr_arena,
        )
    }
}
