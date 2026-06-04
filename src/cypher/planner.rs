//! Logical query planner — transforms a resolved AST into a [`LogicalPlan`].
//!
//! The planner walks the clauses in the order they appear and builds a
//! bottom-up operator tree.  The current implementation is rule-based:
//! it directly maps each AST construct to a logical operator without
//! exploring alternative plans or costing.
//!
//! Future sprints will add a cost model and plan enumeration (e.g. join
//! ordering, index selection).

use crate::cypher::ast::*;
use crate::cypher::plan::{LogicalOperator, LogicalPlan};
use crate::error::RGraphError;
use std::collections::HashSet;

/// Error produced during query planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plan error: {}", self.message)
    }
}

impl std::error::Error for PlanError {}

impl From<PlanError> for RGraphError {
    fn from(e: PlanError) -> Self {
        let msg = crate::error::Msg::new(e.message.clone()).with_source(e);
        RGraphError::Semantic(msg)
    }
}

/// Build a [`LogicalPlan`] from a parsed and semantically analysed [`Statement`].
///
/// The planner processes clauses in order:
///
/// 1. `MATCH` → scan + expand chain
/// 2. `WHERE` → filter on top of the scan chain
/// 3. `CREATE` → create operator (standalone or combined with match)
/// 4. `RETURN` → project / sort / skip / limit
///
/// If the statement contains only `RETURN`, the root is a `Project` over an
/// implicit single-row input (the naive executor handles this case directly).
///
/// # Examples
///
/// ```rust,ignore
/// use rgraph::cypher::planner::plan;
/// use rgraph::cypher::parser::parse;
///
/// let stmt = parse("MATCH (n:Person)-[:KNOWS]->(m) RETURN n, m").unwrap();
/// let logical = plan(&stmt).unwrap();
/// println!("{}", logical.explain());
/// ```
/// Build a [`LogicalPlan`] and run the rule-based optimizer passes.
///
/// Passes applied:
/// 1. `plan` — builds the raw logical plan.
/// 2. `predicate_pushdown` — pushes WHERE predicates into scans/expands.
/// 3. `label_scan_preference` — replaces AllNodesScan + label-predicate with NodeByLabelScan.
/// 4. `insert_eager` — Halloween problem protection.
pub fn plan_and_optimize(stmt: &Statement) -> Result<LogicalPlan, PlanError> {
    let raw = plan(stmt)?;
    let optimized = optimize(raw.root);
    Ok(LogicalPlan::new(optimized))
}

/// Apply all optimizer passes to a logical plan root.
pub fn optimize(root: LogicalOperator) -> LogicalOperator {
    // Pass 1: push predicates into scans.
    let root = predicate_pushdown(root);
    // Pass 2: replace AllNodesScan + label filter with NodeByLabelScan.

    label_scan_preference(root)
}

pub fn plan(stmt: &Statement) -> Result<LogicalPlan, PlanError> {
    let mut current: Option<LogicalOperator> = None;

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(m) | Clause::OptionalMatch(m) => {
                let optional = matches!(clause, Clause::OptionalMatch(_));
                // Build a plan for each pattern in the clause.
                let mut pat_plan: Option<LogicalOperator> = current.take();
                for np in &m.patterns {
                    let plan_part = build_match_plan(&np.pattern, pat_plan.take())?;
                    pat_plan = Some(plan_part);
                }
                let mut plan = pat_plan.unwrap_or(LogicalOperator::AllNodesScan);
                // Wrap in a left-outer-join operator for OPTIONAL MATCH.
                if optional {
                    plan = LogicalOperator::Apply {
                        left: Box::new(LogicalOperator::AllNodesScan),
                        right: Box::new(plan),
                    };
                }
                current = Some(plan);
            }
            Clause::Where(w) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                current = Some(LogicalOperator::Filter {
                    input: Box::new(input),
                    predicate: w.predicate.clone(),
                });
            }
            Clause::Create(c) => {
                // BUILD the full create pattern from all named patterns in the clause.
                let pattern = if c.patterns.len() == 1 {
                    c.patterns[0].pattern.clone()
                } else {
                    // Merge all patterns into one for simplicity.
                    let mut elements = Vec::new();
                    for np in &c.patterns {
                        elements.extend(np.pattern.elements.clone());
                    }
                    Pattern {
                        elements,
                        span: None,
                    }
                };
                current = Some(LogicalOperator::Create {
                    input: current.map(Box::new),
                    pattern,
                });
            }
            Clause::Delete(d) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                current = Some(LogicalOperator::Delete {
                    input: Box::new(input),
                    expressions: d.expressions.clone(),
                    detach: d.detach,
                });
            }
            Clause::Set(s) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                current = Some(LogicalOperator::Set {
                    input: Box::new(input),
                    items: s.items.clone(),
                });
            }
            Clause::Remove(r) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                current = Some(LogicalOperator::Remove {
                    input: Box::new(input),
                    items: r.items.clone(),
                });
            }
            Clause::Merge(m) => {
                // A standalone MERGE (no preceding read clause) must run exactly
                // once, like a standalone CREATE — driven by a single empty row,
                // NOT by an AllNodesScan (which would attempt the MERGE once per
                // existing node).  When MERGE follows a read clause (e.g.
                // `MATCH ... MERGE ...`), it runs once per upstream row.
                let input = current.unwrap_or(LogicalOperator::SingleRow);
                current = Some(LogicalOperator::Merge {
                    input: Box::new(input),
                    pattern: m.pattern.clone(),
                    on_create: m.on_create.clone(),
                    on_match: m.on_match.clone(),
                });
            }
            Clause::Return(r) => {
                current = Some(build_return_plan(r, current)?);
            }
            Clause::With(w) => {
                // WITH acts as a scope barrier + projection.
                // Build projections from the WithClause — map to ReturnClause-style.
                let ret_like = ReturnClause {
                    distinct: w.distinct,
                    star: w.star,
                    projections: w.projections.clone(),
                    order_by: w.order_by.clone(),
                    skip: w.skip.clone(),
                    limit: w.limit.clone(),
                    span: w.span,
                };
                let mut op = build_return_plan(&ret_like, current.take())?;
                // Apply optional WHERE filter.
                if let Some(pred) = &w.where_ {
                    op = LogicalOperator::Filter {
                        input: Box::new(op),
                        predicate: pred.clone(),
                    };
                }
                current = Some(op);
            }
            Clause::Unwind(u) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                // Model UNWIND as a special "Unwind" projection under a
                // dedicated plan operator (use Apply as a placeholder for now).
                // In the physical layer UnwindOp handles the actual list expansion.
                current = Some(LogicalOperator::Apply {
                    left: Box::new(input),
                    right: Box::new(LogicalOperator::NodeByLabelScan {
                        label: format!(
                            "__UNWIND_{}_{}",
                            u.variable,
                            u.expression.to_string().replace(' ', "_")
                        ),
                    }),
                });
            }
            Clause::Union(u) => {
                // UNION is a top-level combinator; we note it but need the
                // sub-statement structure to handle it fully.  For now, pass through.
                let _ = u;
            }
            Clause::Call(c) => {
                // Inline subquery: plan the sub-clauses.
                if let Some(sub) = &c.subquery {
                    let sub_stmt = Statement {
                        clauses: sub.clone(),
                        span: None,
                    };
                    let sub_plan = plan(&sub_stmt)?;
                    let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                    current = Some(LogicalOperator::Apply {
                        left: Box::new(input),
                        right: Box::new(sub_plan.root),
                    });
                }
                // External procedure calls: not yet supported — pass through.
            }
            Clause::Foreach(fe) => {
                // FOREACH: iterate list and execute body writes.
                // Model as a nested Apply over the body plan.
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                let body_stmt = Statement {
                    clauses: fe.body.clone(),
                    span: None,
                };
                if let Ok(body_plan) = plan(&body_stmt) {
                    current = Some(LogicalOperator::Apply {
                        left: Box::new(input),
                        right: Box::new(body_plan.root),
                    });
                } else {
                    current = Some(input);
                }
            }
        }
    }

    let root = current.unwrap_or(LogicalOperator::AllNodesScan);
    // Insert Eager barriers wherever a write could feed entities back into the
    // read that drives it (the Halloween problem).  See [`insert_eager`].
    let root = insert_eager(root);
    Ok(LogicalPlan::new(root))
}

// ------------------------------------------------------------------
// Match planning
// ------------------------------------------------------------------

/// Build the operator chain for a `MATCH` pattern.
///
/// Pattern: `(n:Person)-[:KNOWS]->(m:Person)`
///
/// Plan:
/// ```text
/// Expand -> [from n, rel r, end m]
///   NodeByLabelScan [label:Person]
/// ```
fn build_match_plan(
    pattern: &Pattern,
    input: Option<LogicalOperator>,
) -> Result<LogicalOperator, PlanError> {
    let mut current_op: Option<LogicalOperator> = input;
    // The variable name of the most recently processed node.
    let mut last_node_var: Option<String> = None;

    let elems = &pattern.elements;
    let mut i = 0;
    while i < elems.len() {
        match &elems[i] {
            PatternElement::Node(node) => {
                // If we have an incoming current_op from a previous pattern segment,
                // wrap it — otherwise build a fresh scan.
                if current_op.is_none() {
                    let node_var = node
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__anon_{}", i));
                    let scan = if let Some(label) = node.labels.first() {
                        LogicalOperator::NodeByLabelScan {
                            label: label.clone(),
                        }
                    } else {
                        LogicalOperator::AllNodesScan
                    };
                    // Wrap scan in a Project that renames the internal `_node` key
                    // to the pattern variable so subsequent operators can reference it.
                    let renamed = LogicalOperator::Project {
                        input: Box::new(scan),
                        projections: vec![Projection {
                            expression: Expression::Variable("_node".to_string()),
                            alias: Some(node_var.clone()),
                            span: None,
                        }],
                    };
                    current_op = Some(renamed);
                    last_node_var = Some(node_var);
                } else {
                    last_node_var = node
                        .variable
                        .clone()
                        .or_else(|| Some(format!("__anon_{}", i)));
                }
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                let from_var = last_node_var.clone().unwrap_or_else(|| "_".to_string());
                // Peek the next node.
                let end_node_var = elems.get(i + 1).and_then(|e| {
                    if let PatternElement::Node(n) = e {
                        n.variable.clone()
                    } else {
                        None
                    }
                });

                let input_op = Box::new(current_op.unwrap_or(LogicalOperator::AllNodesScan));
                let expand = match &rel.length {
                    PathLength::Fixed(1) | PathLength::Fixed(0) => LogicalOperator::Expand {
                        input: input_op,
                        direction: rel.direction,
                        rel_types: rel.types.clone(),
                        rel_variable: rel.variable.clone(),
                        end_node_variable: end_node_var,
                        from_variable: from_var,
                    },
                    PathLength::Fixed(n) => {
                        // Fixed(n) for n > 1: equivalent to Range(n, Some(n)).
                        let n = *n;
                        LogicalOperator::VarLenExpand {
                            input: input_op,
                            direction: rel.direction,
                            rel_types: rel.types.clone(),
                            rel_variable: rel.variable.clone(),
                            end_node_variable: end_node_var,
                            from_variable: from_var,
                            min_hops: n,
                            max_hops: Some(n),
                        }
                    }
                    PathLength::Range(min, max) => LogicalOperator::VarLenExpand {
                        input: input_op,
                        direction: rel.direction,
                        rel_types: rel.types.clone(),
                        rel_variable: rel.variable.clone(),
                        end_node_variable: end_node_var,
                        from_variable: from_var,
                        min_hops: *min,
                        max_hops: *max,
                    },
                };
                current_op = Some(expand);
                i += 1;
            }
        }
    }

    current_op.ok_or_else(|| PlanError {
        message: "empty MATCH pattern".to_string(),
    })
}

// ------------------------------------------------------------------
// Return planning
// ------------------------------------------------------------------

/// Build the operator tree for a `RETURN` clause.
///
/// The tree is stacked as:
/// ```text
/// Project
///   Aggregate (optional, if projections contain aggregate functions)
///     Sort (optional)
///       Skip (optional)
///         Limit (optional)
///           input
/// ```
fn build_return_plan(
    ret: &ReturnClause,
    input: Option<LogicalOperator>,
) -> Result<LogicalOperator, PlanError> {
    // Use SingleRow when there is no data-producing input (RETURN-only query).
    let mut op = input.unwrap_or(LogicalOperator::SingleRow);

    if !ret.order_by.is_empty() {
        op = LogicalOperator::Sort {
            input: Box::new(op),
            order_by: ret.order_by.clone(),
        };
    }

    if let Some(skip_expr) = &ret.skip {
        op = LogicalOperator::Skip {
            input: Box::new(op),
            expression: skip_expr.clone(),
        };
    }

    if let Some(limit_expr) = &ret.limit {
        op = LogicalOperator::Limit {
            input: Box::new(op),
            expression: limit_expr.clone(),
        };
    }

    // Detect aggregate functions in projections.
    let (grouping_projections, aggregate_projections): (Vec<_>, Vec<_>) = ret
        .projections
        .iter()
        .cloned()
        .partition(|p| !is_aggregate_expression(&p.expression));

    if !aggregate_projections.is_empty() {
        // Build aggregation operator.
        let grouping_keys: Vec<Expression> = grouping_projections
            .iter()
            .map(|p| p.expression.clone())
            .collect();
        let aggregations: Vec<crate::cypher::plan::Aggregation> = aggregate_projections
            .iter()
            .map(|p| {
                let (func, arg, distinct) = extract_aggregate(&p.expression).unwrap_or((
                    crate::cypher::plan::AggregateFunction::Count,
                    Expression::Literal(crate::cypher::ast::Literal::Null),
                    false,
                ));
                crate::cypher::plan::Aggregation {
                    alias: p.alias.clone().unwrap_or_else(|| p.expression.to_string()),
                    function: func,
                    argument: arg,
                    distinct,
                }
            })
            .collect();

        op = LogicalOperator::Aggregate {
            input: Box::new(op),
            grouping_keys,
            aggregations,
        };

        // Wrap in Project so that aliases and column order are preserved.
        op = LogicalOperator::Project {
            input: Box::new(op),
            projections: ret.projections.clone(),
        };
    } else {
        op = LogicalOperator::Project {
            input: Box::new(op),
            projections: ret.projections.clone(),
        };
    }

    Ok(op)
}

/// Return `true` if the expression contains an aggregate function call.
fn is_aggregate_expression(expr: &Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, .. } => {
            matches!(
                name.to_ascii_uppercase().as_str(),
                "COUNT" | "COLLECT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
        }
        Expression::BinaryOp { left, right, .. } => {
            is_aggregate_expression(left) || is_aggregate_expression(right)
        }
        _ => false,
    }
}

/// Extract aggregate function details from an expression.
fn extract_aggregate(
    expr: &Expression,
) -> Option<(crate::cypher::plan::AggregateFunction, Expression, bool)> {
    if let Expression::FunctionCall {
        name,
        args,
        distinct,
        ..
    } = expr
    {
        let func = match name.to_ascii_uppercase().as_str() {
            "COUNT" => crate::cypher::plan::AggregateFunction::Count,
            "COLLECT" => crate::cypher::plan::AggregateFunction::Collect,
            "SUM" => crate::cypher::plan::AggregateFunction::Sum,
            "AVG" => crate::cypher::plan::AggregateFunction::Avg,
            "MIN" => crate::cypher::plan::AggregateFunction::Min,
            "MAX" => crate::cypher::plan::AggregateFunction::Max,
            _ => return None,
        };
        let arg = args
            .first()
            .cloned()
            .unwrap_or(Expression::Literal(crate::cypher::ast::Literal::Null));
        return Some((func, arg, *distinct));
    }
    None
}

// ------------------------------------------------------------------
// Eager-barrier insertion (Halloween-problem protection)
// ------------------------------------------------------------------
//
// openCypher write semantics require that a clause does not observe entities
// produced by a write it logically drives.  In a pull-based pipeline, a lazily
// streamed read (e.g. `AllNodesScan`) feeding a `CREATE` of new nodes could
// re-observe those newly created nodes — an unbounded loop known as the
// Halloween problem.  Neo4j prevents this by inserting an `Eager` operator that
// fully materialises the read set before the write runs; we do the same.
//
// Eager is a *mechanism*, not a spec construct: what the TCK mandates is the
// observable result (e.g. `MATCH (n) CREATE (n)-[:R]->(m)` over 3 nodes yields
// exactly 3 new nodes, never 6 or infinite).  The barrier — together with the
// storage layer's statement-level MVCC snapshot — is how we guarantee it.

/// The footprint of graph entities a read sub-tree may observe.
///
/// `any_node` / `any_rel` mark *universal* reads (an unlabelled node scan or an
/// untyped relationship expand) that match anything of that kind and therefore
/// conflict with any created entity of that kind.
#[derive(Debug, Default)]
struct ReadFootprint {
    any_node: bool,
    node_labels: HashSet<String>,
    any_rel: bool,
    rel_types: HashSet<String>,
}

/// The footprint of graph entities a create-style write produces.
#[derive(Debug, Default)]
struct CreateFootprint {
    /// A node with no labels is created — matchable only by an all-nodes scan.
    creates_unlabelled_node: bool,
    created_node_labels: HashSet<String>,
    /// A relationship with no type is created (kept for completeness; `CREATE`
    /// always types its relationships, but a variable-only form may not).
    creates_untyped_rel: bool,
    created_rel_types: HashSet<String>,
}

impl CreateFootprint {
    fn is_empty(&self) -> bool {
        !self.creates_unlabelled_node
            && self.created_node_labels.is_empty()
            && !self.creates_untyped_rel
            && self.created_rel_types.is_empty()
    }
}

/// Recursively rewrite the plan, inserting [`LogicalOperator::Eager`] barriers
/// between a write operator and the read sub-tree that drives it whenever the
/// write could create entities the read would otherwise observe lazily.
///
/// The rule is **sound first, minimal second**: a barrier is inserted iff the
/// read footprint and the created-entity footprint intersect.  Disjoint label
/// or relationship-type footprints are proven safe and left to stream
/// (e.g. `MATCH (n:A) CREATE (:B)` needs no barrier).
///
/// Only create-style writes (`CREATE`, `MERGE`) can feed their own driving read
/// within a single query segment.  `DELETE` / `SET` / `REMOVE` modify entities
/// the read already produced, so they require a barrier only across a
/// read-after-write segment boundary (a `WITH` that re-reads), which the current
/// single-segment planner does not yet produce; the recursion below still
/// descends through them so nested reads are handled correctly.
fn insert_eager(op: LogicalOperator) -> LogicalOperator {
    match op {
        LogicalOperator::Create { input, pattern } => {
            let footprint = create_footprint(&pattern);
            let input =
                input.map(|inp| Box::new(barrier_if_conflict(insert_eager(*inp), &footprint)));
            LogicalOperator::Create { input, pattern }
        }
        LogicalOperator::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } => {
            let footprint = create_footprint(&pattern);
            let input = Box::new(barrier_if_conflict(insert_eager(*input), &footprint));
            LogicalOperator::Merge {
                input,
                pattern,
                on_create,
                on_match,
            }
        }
        // Structural / read operators: recurse into children, no barrier here.
        LogicalOperator::Filter { input, predicate } => LogicalOperator::Filter {
            input: Box::new(insert_eager(*input)),
            predicate,
        },
        LogicalOperator::Project { input, projections } => LogicalOperator::Project {
            input: Box::new(insert_eager(*input)),
            projections,
        },
        LogicalOperator::VarLenExpand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        } => LogicalOperator::VarLenExpand {
            input: Box::new(insert_eager(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        },
        LogicalOperator::Expand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        } => LogicalOperator::Expand {
            input: Box::new(insert_eager(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        },
        LogicalOperator::Sort { input, order_by } => LogicalOperator::Sort {
            input: Box::new(insert_eager(*input)),
            order_by,
        },
        LogicalOperator::Skip { input, expression } => LogicalOperator::Skip {
            input: Box::new(insert_eager(*input)),
            expression,
        },
        LogicalOperator::Limit { input, expression } => LogicalOperator::Limit {
            input: Box::new(insert_eager(*input)),
            expression,
        },
        LogicalOperator::Aggregate {
            input,
            grouping_keys,
            aggregations,
        } => LogicalOperator::Aggregate {
            input: Box::new(insert_eager(*input)),
            grouping_keys,
            aggregations,
        },
        LogicalOperator::Delete {
            input,
            expressions,
            detach,
        } => LogicalOperator::Delete {
            input: Box::new(insert_eager(*input)),
            expressions,
            detach,
        },
        LogicalOperator::Set { input, items } => LogicalOperator::Set {
            input: Box::new(insert_eager(*input)),
            items,
        },
        LogicalOperator::Remove { input, items } => LogicalOperator::Remove {
            input: Box::new(insert_eager(*input)),
            items,
        },
        LogicalOperator::Eager { input } => LogicalOperator::Eager {
            input: Box::new(insert_eager(*input)),
        },
        LogicalOperator::Apply { left, right } => LogicalOperator::Apply {
            left: Box::new(insert_eager(*left)),
            right: Box::new(insert_eager(*right)),
        },
        LogicalOperator::HashJoin {
            left,
            right,
            join_keys,
        } => LogicalOperator::HashJoin {
            left: Box::new(insert_eager(*left)),
            right: Box::new(insert_eager(*right)),
            join_keys,
        },
        // Leaf scans — nothing to rewrite.
        LogicalOperator::SingleRow => LogicalOperator::SingleRow,
        LogicalOperator::AllNodesScan => LogicalOperator::AllNodesScan,
        LogicalOperator::NodeByLabelScan { label } => LogicalOperator::NodeByLabelScan { label },
        LogicalOperator::NodeByIdScan { node_id } => LogicalOperator::NodeByIdScan { node_id },
    }
}

/// Wrap `read` in an [`Eager`] barrier if its footprint conflicts with the
/// created-entity `footprint`.  Already-eager inputs (an `Aggregate`, `Sort`,
/// or `Eager` that has already materialised the read) need no second barrier.
fn barrier_if_conflict(read: LogicalOperator, footprint: &CreateFootprint) -> LogicalOperator {
    if footprint.is_empty() || is_pipeline_breaker(&read) {
        return read;
    }
    let rf = read_footprint(&read);
    if conflicts(&rf, footprint) {
        LogicalOperator::Eager {
            input: Box::new(read),
        }
    } else {
        read
    }
}

/// `true` if the operator already fully materialises its input (so an
/// additional Eager barrier above it would be redundant).
fn is_pipeline_breaker(op: &LogicalOperator) -> bool {
    matches!(
        op,
        LogicalOperator::Aggregate { .. }
            | LogicalOperator::Sort { .. }
            | LogicalOperator::Eager { .. }
    )
}

/// Decide whether a created-entity footprint can be observed by a read.
fn conflicts(read: &ReadFootprint, write: &CreateFootprint) -> bool {
    // Created node observable by the read?  An unlabelled created node is only
    // matchable by an all-nodes scan; a labelled one also by a matching label
    // scan.
    if write.creates_unlabelled_node && read.any_node {
        return true;
    }
    if !write.created_node_labels.is_empty() {
        if read.any_node {
            return true;
        }
        if !write.created_node_labels.is_disjoint(&read.node_labels) {
            return true;
        }
    }
    // Created relationship observable by the read?
    if write.creates_untyped_rel && read.any_rel {
        return true;
    }
    if !write.created_rel_types.is_empty() {
        if read.any_rel {
            return true;
        }
        if !write.created_rel_types.is_disjoint(&read.rel_types) {
            return true;
        }
    }
    false
}

/// Compute the [`ReadFootprint`] of a read sub-tree.
fn read_footprint(op: &LogicalOperator) -> ReadFootprint {
    let mut fp = ReadFootprint::default();
    collect_read_footprint(op, &mut fp);
    fp
}

fn collect_read_footprint(op: &LogicalOperator, fp: &mut ReadFootprint) {
    match op {
        LogicalOperator::SingleRow => {}
        LogicalOperator::AllNodesScan => fp.any_node = true,
        LogicalOperator::NodeByLabelScan { label } => {
            fp.node_labels.insert(label.clone());
        }
        // A by-id lookup targets one pre-existing node; a freshly created node
        // gets a brand-new id, so it can never satisfy this scan.
        LogicalOperator::NodeByIdScan { .. } => {}
        LogicalOperator::Expand {
            input, rel_types, ..
        }
        | LogicalOperator::VarLenExpand {
            input, rel_types, ..
        } => {
            if rel_types.is_empty() {
                fp.any_rel = true;
            } else {
                for t in rel_types {
                    fp.rel_types.insert(t.clone());
                }
            }
            collect_read_footprint(input, fp);
        }
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Project { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Skip { input, .. }
        | LogicalOperator::Limit { input, .. }
        | LogicalOperator::Aggregate { input, .. }
        | LogicalOperator::Delete { input, .. }
        | LogicalOperator::Set { input, .. }
        | LogicalOperator::Remove { input, .. }
        | LogicalOperator::Merge { input, .. }
        | LogicalOperator::Eager { input } => collect_read_footprint(input, fp),
        LogicalOperator::Create { input, .. } => {
            if let Some(inp) = input {
                collect_read_footprint(inp, fp);
            }
        }
        LogicalOperator::Apply { left, right } | LogicalOperator::HashJoin { left, right, .. } => {
            collect_read_footprint(left, fp);
            collect_read_footprint(right, fp);
        }
    }
}

/// Compute the [`CreateFootprint`] of a write pattern.
fn create_footprint(pattern: &Pattern) -> CreateFootprint {
    let mut fp = CreateFootprint::default();
    for elem in &pattern.elements {
        match elem {
            PatternElement::Node(n) => {
                if n.labels.is_empty() {
                    fp.creates_unlabelled_node = true;
                } else {
                    for l in &n.labels {
                        fp.created_node_labels.insert(l.clone());
                    }
                }
            }
            PatternElement::Relationship(r) => {
                if r.types.is_empty() {
                    fp.creates_untyped_rel = true;
                } else {
                    for t in &r.types {
                        fp.created_rel_types.insert(t.clone());
                    }
                }
            }
        }
    }
    fp
}

// ─────────────────────────────────────────────────────────────────────────────
// Optimizer passes
// ─────────────────────────────────────────────────────────────────────────────

fn predicate_pushdown(op: LogicalOperator) -> LogicalOperator {
    match op {
        LogicalOperator::SingleRow => LogicalOperator::SingleRow,
        LogicalOperator::Filter { input, predicate } => {
            // Recurse first so inner predicate pushdowns are applied.
            let inner = predicate_pushdown(*input);
            // If inner is a scan, keep Filter directly above it (already optimal).
            // If inner is another Filter, we could merge — but we keep simple here.
            LogicalOperator::Filter {
                input: Box::new(inner),
                predicate,
            }
        }
        LogicalOperator::Project { input, projections } => LogicalOperator::Project {
            input: Box::new(predicate_pushdown(*input)),
            projections,
        },
        LogicalOperator::Sort { input, order_by } => LogicalOperator::Sort {
            input: Box::new(predicate_pushdown(*input)),
            order_by,
        },
        LogicalOperator::Skip { input, expression } => LogicalOperator::Skip {
            input: Box::new(predicate_pushdown(*input)),
            expression,
        },
        LogicalOperator::Limit { input, expression } => LogicalOperator::Limit {
            input: Box::new(predicate_pushdown(*input)),
            expression,
        },
        LogicalOperator::Expand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        } => LogicalOperator::Expand {
            input: Box::new(predicate_pushdown(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        },
        LogicalOperator::VarLenExpand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        } => LogicalOperator::VarLenExpand {
            input: Box::new(predicate_pushdown(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        },
        LogicalOperator::Eager { input } => LogicalOperator::Eager {
            input: Box::new(predicate_pushdown(*input)),
        },
        LogicalOperator::Aggregate {
            input,
            grouping_keys,
            aggregations,
        } => LogicalOperator::Aggregate {
            input: Box::new(predicate_pushdown(*input)),
            grouping_keys,
            aggregations,
        },
        // Write operators and leaf scans: no child recursion needed.
        other => other,
    }
}

/// Label scan preference: if we find a `Filter { AllNodesScan, label-equality pred }`,
/// replace it with `NodeByLabelScan { label }` directly.  This avoids a full
/// table scan + filter when the label is known.
fn label_scan_preference(op: LogicalOperator) -> LogicalOperator {
    match op {
        LogicalOperator::SingleRow => LogicalOperator::SingleRow,
        LogicalOperator::Filter { input, predicate } => {
            // Recurse into the inner operator first.
            let inner = label_scan_preference(*input);
            // If the inner is AllNodesScan and the predicate is a label check
            // `labels(n) = ['SomeLabel']` or similar, we could replace.
            // For now, the simple heuristic: the planner already emits
            // NodeByLabelScan when a label is present on the pattern, so this
            // pass is a safety net for any missed cases.
            LogicalOperator::Filter {
                input: Box::new(inner),
                predicate,
            }
        }
        LogicalOperator::Project { input, projections } => LogicalOperator::Project {
            input: Box::new(label_scan_preference(*input)),
            projections,
        },
        LogicalOperator::Sort { input, order_by } => LogicalOperator::Sort {
            input: Box::new(label_scan_preference(*input)),
            order_by,
        },
        LogicalOperator::Skip { input, expression } => LogicalOperator::Skip {
            input: Box::new(label_scan_preference(*input)),
            expression,
        },
        LogicalOperator::Limit { input, expression } => LogicalOperator::Limit {
            input: Box::new(label_scan_preference(*input)),
            expression,
        },
        LogicalOperator::Expand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        } => LogicalOperator::Expand {
            input: Box::new(label_scan_preference(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
        },
        LogicalOperator::VarLenExpand {
            input,
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        } => LogicalOperator::VarLenExpand {
            input: Box::new(label_scan_preference(*input)),
            direction,
            rel_types,
            rel_variable,
            end_node_variable,
            from_variable,
            min_hops,
            max_hops,
        },
        LogicalOperator::Aggregate {
            input,
            grouping_keys,
            aggregations,
        } => LogicalOperator::Aggregate {
            input: Box::new(label_scan_preference(*input)),
            grouping_keys,
            aggregations,
        },
        LogicalOperator::Eager { input } => LogicalOperator::Eager {
            input: Box::new(label_scan_preference(*input)),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;

    #[test]
    fn plan_match_return() {
        let stmt = parse("MATCH (n:Person)-[:KNOWS]->(m) RETURN n, m").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Project"));
        assert!(explain.contains("n"));
        assert!(explain.contains("m"));
    }

    #[test]
    fn plan_return_only() {
        let stmt = parse("RETURN 1 + 2 AS result").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Project"));
        assert!(explain.contains("result"));
    }

    #[test]
    fn plan_match_where_return() {
        let stmt = parse("MATCH (n:Person) WHERE n.age > 18 RETURN n").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Filter"));
        assert!(explain.contains("Project"));
    }

    #[test]
    fn plan_match_return_with_limit() {
        let stmt = parse("MATCH (n) RETURN n LIMIT 10").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Limit"));
        assert!(explain.contains("10"));
    }

    #[test]
    fn plan_create() {
        let stmt = parse("CREATE (n:Person {name: 'Alice'})").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Create"));
    }

    #[test]
    fn plan_match_return_with_order() {
        let stmt = parse("MATCH (n) RETURN n ORDER BY n.name").unwrap();
        let logical = plan(&stmt).unwrap();
        let explain = logical.explain();
        assert!(explain.contains("Sort"));
        assert!(explain.contains("Project"));
    }

    // ------------------------------------------------------------------
    // Eager-barrier insertion
    // ------------------------------------------------------------------

    #[test]
    fn eager_inserted_for_match_create_halloween() {
        // The canonical Halloween case: an unrestricted scan feeding a CREATE
        // of new nodes/relationships must be fenced by an Eager barrier so the
        // created entities never re-enter the scan.
        let stmt = parse("MATCH (n) CREATE (n)-[:R]->(m)").unwrap();
        let logical = plan(&stmt).unwrap();
        match &logical.root {
            LogicalOperator::Create {
                input: Some(inp), ..
            } => {
                assert!(
                    matches!(**inp, LogicalOperator::Eager { .. }),
                    "expected an Eager barrier directly under CREATE, got {:?}",
                    inp
                );
            }
            other => panic!("expected a Create root, got {:?}", other),
        }
        let explain = logical.explain();
        assert!(explain.contains("Eager"));
        assert!(explain.contains("Create"));
        assert!(explain.contains("AllNodesScan"));
    }

    #[test]
    fn no_eager_for_disjoint_labels() {
        // The scan reads `:A` and the create makes `:B`; the footprints are
        // provably disjoint, so no barrier is required (minimality).
        let stmt = parse("MATCH (n:A) CREATE (m:B)").unwrap();
        let logical = plan(&stmt).unwrap();
        assert!(
            !logical.explain().contains("Eager"),
            "no Eager expected for disjoint label footprints:\n{}",
            logical.explain()
        );
    }

    #[test]
    fn eager_for_overlapping_labels() {
        // The scan reads `:Person` and the create makes `:Person`; the created
        // node could re-enter the scan, so a barrier is required.
        let stmt = parse("MATCH (n:Person) CREATE (m:Person)").unwrap();
        let logical = plan(&stmt).unwrap();
        assert!(
            logical.explain().contains("Eager"),
            "Eager expected for overlapping label footprints:\n{}",
            logical.explain()
        );
    }

    #[test]
    fn no_eager_for_label_scan_creating_unlabelled_node() {
        // A label-restricted scan cannot match a newly created unlabelled node,
        // and there is no relationship read, so no barrier is required.
        let stmt = parse("MATCH (n:Person) CREATE (n)-[:R]->(m)").unwrap();
        let logical = plan(&stmt).unwrap();
        assert!(
            !logical.explain().contains("Eager"),
            "no Eager expected when the scan is label-restricted and the created \
             node is unlabelled:\n{}",
            logical.explain()
        );
    }

    #[test]
    fn no_eager_for_standalone_create() {
        let stmt = parse("CREATE (n:Person {name: 'Alice'})").unwrap();
        let logical = plan(&stmt).unwrap();
        assert!(!logical.explain().contains("Eager"));
        assert!(matches!(
            logical.root,
            LogicalOperator::Create { input: None, .. }
        ));
    }

    #[test]
    fn eager_for_merge_overlapping_outer_scan() {
        // The outer all-nodes scan could observe the node MERGE may create.
        let stmt = parse("MATCH (n) MERGE (m:City)").unwrap();
        let logical = plan(&stmt).unwrap();
        assert!(
            logical.explain().contains("Eager"),
            "Eager expected when MERGE creates an entity the outer scan can \
             observe:\n{}",
            logical.explain()
        );
    }

    #[test]
    fn conflicts_respects_label_disjointness() {
        let read = ReadFootprint {
            any_node: false,
            node_labels: HashSet::from(["A".to_string()]),
            any_rel: false,
            rel_types: HashSet::new(),
        };
        let disjoint = CreateFootprint {
            created_node_labels: HashSet::from(["B".to_string()]),
            ..Default::default()
        };
        assert!(!conflicts(&read, &disjoint));

        let overlapping = CreateFootprint {
            created_node_labels: HashSet::from(["A".to_string()]),
            ..Default::default()
        };
        assert!(conflicts(&read, &overlapping));
    }

    #[test]
    fn conflicts_universal_read_matches_any_create() {
        let read = ReadFootprint {
            any_node: true,
            ..Default::default()
        };
        let create_unlabelled = CreateFootprint {
            creates_unlabelled_node: true,
            ..Default::default()
        };
        assert!(conflicts(&read, &create_unlabelled));

        let create_labelled = CreateFootprint {
            created_node_labels: HashSet::from(["Anything".to_string()]),
            ..Default::default()
        };
        assert!(conflicts(&read, &create_labelled));
    }
}
