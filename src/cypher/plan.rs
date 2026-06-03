//! Logical query plan — abstract operator tree produced by the planner.
//!
//! A logical plan describes *what* the query engine must compute, not *how*.
//! Each operator is a node in a tree; data flows from leaves (scans) up to
//! the root (the final projection).  The physical execution engine will
//! later map this tree to concrete iterator-based operators.

use crate::cypher::ast::*;

/// A complete logical plan for one query.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalPlan {
    /// The root operator of the plan tree.
    pub root: LogicalOperator,
    /// Estimated number of rows the root will produce (stub until cost model).
    pub estimated_cardinality: u64,
}

/// Operator in the logical plan tree.
///
/// Every operator produces a stream of rows.  Each row is a map of
/// variable names to [`Value`](crate::cypher::value::Value).
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOperator {
    /// Scan every node in the graph.
    AllNodesScan,

    /// Scan nodes that have a specific label.
    NodeByLabelScan { label: String },

    /// Scan a single node by its internal ID (used for point lookups).
    NodeByIdScan { node_id: u64 },

    /// Expand relationships from the input nodes.
    ///
    /// Produces one output row per relationship found, binding the
    /// relationship variable (if any) and the endpoint node variable.
    Expand {
        input: Box<LogicalOperator>,
        direction: Direction,
        rel_types: Vec<String>,
        rel_variable: Option<String>,
        end_node_variable: Option<String>,
        // The variable in the input row that holds the start node.
        from_variable: String,
    },

    /// Apply a predicate to each input row and keep only matching rows.
    Filter {
        input: Box<LogicalOperator>,
        predicate: Expression,
    },

    /// Compute projections and produce output columns.
    Project {
        input: Box<LogicalOperator>,
        projections: Vec<Projection>,
    },

    /// Sort the input rows according to one or more order keys.
    Sort {
        input: Box<LogicalOperator>,
        order_by: Vec<OrderItem>,
    },

    /// Discard the first N rows.
    Skip {
        input: Box<LogicalOperator>,
        expression: Expression,
    },

    /// Keep only the first N rows.
    Limit {
        input: Box<LogicalOperator>,
        expression: Expression,
    },

    /// Create new nodes and/or relationships.
    ///
    /// A standalone `CREATE` has no `input`.  When the clause is preceded by a
    /// reading clause (`MATCH ... CREATE ...`) the matched rows flow in through
    /// `input`, and the `CREATE` runs once per incoming row.
    Create {
        input: Option<Box<LogicalOperator>>,
        pattern: Pattern,
    },

    /// Eager barrier — fully materialise the input rows before yielding any
    /// row to the parent operator.
    ///
    /// The planner inserts this between a read sub-tree and a write operator
    /// when the write could produce entities the read would otherwise observe
    /// while still streaming (the "Halloween problem").  Materialising the read
    /// set freezes it, so newly created nodes/relationships cannot re-enter the
    /// scan that is driving the write.  The barrier is transparent: it yields
    /// exactly the rows it buffered, in order.
    Eager { input: Box<LogicalOperator> },

    /// Delete nodes and/or relationships.
    Delete {
        input: Box<LogicalOperator>,
        expressions: Vec<Expression>,
        detach: bool,
    },

    /// Set properties or labels on existing entities.
    Set {
        input: Box<LogicalOperator>,
        items: Vec<SetItem>,
    },

    /// Remove properties or labels from existing entities.
    Remove {
        input: Box<LogicalOperator>,
        items: Vec<RemoveItem>,
    },

    /// MERGE pattern with ON CREATE / ON MATCH actions.
    Merge {
        input: Box<LogicalOperator>,
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },

    /// Nested-loop apply for sub-queries (stub for Sprint 21).
    Apply {
        left: Box<LogicalOperator>,
        right: Box<LogicalOperator>,
    },

    /// Hash join between two sub-plans (stub for Sprint 21).
    HashJoin {
        left: Box<LogicalOperator>,
        right: Box<LogicalOperator>,
        join_keys: Vec<String>,
    },

    /// Aggregate with optional implicit grouping keys.
    Aggregate {
        input: Box<LogicalOperator>,
        /// Expressions that form the grouping keys (implicit or explicit).
        grouping_keys: Vec<Expression>,
        /// Aggregate projections: (alias, function_name, argument_expression, distinct).
        aggregations: Vec<Aggregation>,
    },
}

/// One aggregate expression in an `Aggregate` operator.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregation {
    pub alias: String,
    pub function: AggregateFunction,
    pub argument: Expression,
    pub distinct: bool,
}

/// Supported aggregate functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    Collect,
    Sum,
    Avg,
    Min,
    Max,
}

impl LogicalPlan {
    /// Create a new plan with the given root operator.
    pub fn new(root: LogicalOperator) -> Self {
        Self {
            root,
            estimated_cardinality: 0,
        }
    }

    /// Produce a human-readable EXPLAIN representation.
    pub fn explain(&self) -> String {
        let mut out = String::new();
        out.push_str("Logical Plan\n");
        out.push_str("============\n");
        self.root.explain_inner(&mut out, 0);
        out.push_str(&format!("\nEstimated cardinality: {}\n", self.estimated_cardinality));
        out
    }
}

impl LogicalOperator {
    fn explain_inner(&self, out: &mut String, depth: usize) {
        let indent = "  ".repeat(depth);
        match self {
            LogicalOperator::AllNodesScan => {
                out.push_str(&format!("{}AllNodesScan\n", indent));
            }
            LogicalOperator::NodeByLabelScan { label } => {
                out.push_str(&format!("{}NodeByLabelScan [label:{}]\n", indent, label));
            }
            LogicalOperator::NodeByIdScan { node_id } => {
                out.push_str(&format!("{}NodeByIdScan [id:{}]\n", indent, node_id));
            }
            LogicalOperator::Expand {
                input,
                direction,
                rel_types,
                rel_variable,
                end_node_variable,
                from_variable,
            } => {
                let dir_str = match direction {
                    Direction::Outgoing => "->",
                    Direction::Incoming => "<-",
                    Direction::Both => "--",
                };
                let types_str = if rel_types.is_empty() {
                    "ANY".to_string()
                } else {
                    rel_types.join(":")
                };
                out.push_str(&format!(
                    "{}Expand {} [:{}] from {}\n",
                    indent, dir_str, types_str, from_variable
                ));
                if let Some(rv) = rel_variable {
                    out.push_str(&format!("{}  rel: {}\n", indent, rv));
                }
                if let Some(ev) = end_node_variable {
                    out.push_str(&format!("{}  end: {}\n", indent, ev));
                }
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Filter { input, predicate } => {
                out.push_str(&format!("{}Filter [{}]\n", indent, predicate));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Project { input, projections } => {
                let items: Vec<String> = projections.iter().map(|p| p.to_string()).collect();
                out.push_str(&format!("{}Project [{}]\n", indent, items.join(", ")));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Sort { input, order_by } => {
                let items: Vec<String> = order_by.iter().map(|o| o.to_string()).collect();
                out.push_str(&format!("{}Sort [{}]\n", indent, items.join(", ")));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Skip { input, expression } => {
                out.push_str(&format!("{}Skip [{}]\n", indent, expression));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Limit { input, expression } => {
                out.push_str(&format!("{}Limit [{}]\n", indent, expression));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Create { input, pattern } => {
                out.push_str(&format!("{}Create [{}]\n", indent, pattern));
                if let Some(inp) = input {
                    inp.explain_inner(out, depth + 1);
                }
            }
            LogicalOperator::Eager { input } => {
                out.push_str(&format!("{}Eager\n", indent));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Delete { input, .. } => {
                out.push_str(&format!("{}Delete\n", indent));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Set { input, .. } => {
                out.push_str(&format!("{}Set\n", indent));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Remove { input, .. } => {
                out.push_str(&format!("{}Remove\n", indent));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Merge { input, .. } => {
                out.push_str(&format!("{}Merge\n", indent));
                input.explain_inner(out, depth + 1);
            }
            LogicalOperator::Apply { left, right } => {
                out.push_str(&format!("{}Apply\n", indent));
                left.explain_inner(out, depth + 1);
                right.explain_inner(out, depth + 1);
            }
            LogicalOperator::HashJoin {
                left,
                right,
                join_keys,
            } => {
                out.push_str(&format!(
                    "{}HashJoin [keys:{}]\n",
                    indent,
                    join_keys.join(", ")
                ));
                left.explain_inner(out, depth + 1);
                right.explain_inner(out, depth + 1);
            }
            LogicalOperator::Aggregate {
                input,
                grouping_keys,
                aggregations,
            } => {
                let funcs: Vec<String> = aggregations
                    .iter()
                    .map(|a| format!("{}({})", a.function, a.argument))
                    .collect();
                out.push_str(&format!(
                    "{}Aggregate [group:{}] [agg:{}]\n",
                    indent,
                    grouping_keys.len(),
                    funcs.join(", ")
                ));
                input.explain_inner(out, depth + 1);
            }
        }
    }

    /// Return the set of variable names produced by this operator.
    pub fn output_variables(&self) -> Vec<String> {
        let mut vars = Vec::new();
        match self {
            LogicalOperator::AllNodesScan => {}
            LogicalOperator::NodeByLabelScan { .. } => {}
            LogicalOperator::NodeByIdScan { .. } => {}
            LogicalOperator::Expand {
                rel_variable,
                end_node_variable,
                ..
            } => {
                if let Some(rv) = rel_variable {
                    vars.push(rv.clone());
                }
                if let Some(ev) = end_node_variable {
                    vars.push(ev.clone());
                }
            }
            LogicalOperator::Filter { .. } => {}
            LogicalOperator::Project { projections, .. } => {
                for p in projections {
                    let name = p
                        .alias
                        .clone()
                        .unwrap_or_else(|| p.expression.to_string());
                    vars.push(name);
                }
            }
            LogicalOperator::Aggregate { grouping_keys, aggregations, .. } => {
                for gk in grouping_keys {
                    collect_expression_variables(gk, &mut vars);
                }
                for agg in aggregations {
                    vars.push(agg.alias.clone());
                }
            }
            LogicalOperator::Sort { .. } => {}
            LogicalOperator::Skip { .. } => {}
            LogicalOperator::Limit { .. } => {}
            LogicalOperator::Create { input, pattern } => {
                if let Some(inp) = input {
                    vars.extend(inp.output_variables());
                }
                for elem in &pattern.elements {
                    if let PatternElement::Node(n) = elem {
                        if let Some(v) = &n.variable {
                            if !vars.contains(v) {
                                vars.push(v.clone());
                            }
                        }
                    }
                    if let PatternElement::Relationship(r) = elem {
                        if let Some(v) = &r.variable {
                            if !vars.contains(v) {
                                vars.push(v.clone());
                            }
                        }
                    }
                }
            }
            LogicalOperator::Eager { input } => {
                vars.extend(input.output_variables());
            }
            LogicalOperator::Delete { .. } => {}
            LogicalOperator::Set { .. } => {}
            LogicalOperator::Remove { .. } => {}
            LogicalOperator::Merge { .. } => {}
            LogicalOperator::Apply { left, right } => {
                vars.extend(left.output_variables());
                vars.extend(right.output_variables());
            }
            LogicalOperator::HashJoin { left, .. } => {
                vars.extend(left.output_variables());
            }
        }
        vars
    }

    /// Return the set of variable names required by this operator.
    pub fn required_variables(&self) -> Vec<String> {
        let mut vars = Vec::new();
        match self {
            LogicalOperator::AllNodesScan => {}
            LogicalOperator::NodeByLabelScan { .. } => {}
            LogicalOperator::NodeByIdScan { .. } => {}
            LogicalOperator::Expand { from_variable, .. } => {
                vars.push(from_variable.clone());
            }
            LogicalOperator::Filter { predicate, .. } => {
                collect_expression_variables(predicate, &mut vars);
            }
            LogicalOperator::Project { projections, .. } => {
                for p in projections {
                    collect_expression_variables(&p.expression, &mut vars);
                }
            }
            LogicalOperator::Aggregate { grouping_keys, aggregations, .. } => {
                for gk in grouping_keys {
                    collect_expression_variables(gk, &mut vars);
                }
                for agg in aggregations {
                    collect_expression_variables(&agg.argument, &mut vars);
                }
            }
            LogicalOperator::Sort { order_by, .. } => {
                for item in order_by {
                    collect_expression_variables(&item.expression, &mut vars);
                }
            }
            LogicalOperator::Skip { expression, .. } => {
                collect_expression_variables(expression, &mut vars);
            }
            LogicalOperator::Limit { expression, .. } => {
                collect_expression_variables(expression, &mut vars);
            }
            LogicalOperator::Create { input, .. } => {
                if let Some(inp) = input {
                    vars.extend(inp.required_variables());
                }
            }
            LogicalOperator::Eager { input } => {
                vars.extend(input.required_variables());
            }
            LogicalOperator::Delete { expressions, .. } => {
                for expr in expressions {
                    collect_expression_variables(expr, &mut vars);
                }
            }
            LogicalOperator::Set { items, .. } => {
                for item in items {
                    match item {
                        SetItem::Property { target, value } => {
                            collect_expression_variables(target, &mut vars);
                            collect_expression_variables(value, &mut vars);
                        }
                        SetItem::Label { variable, .. } => {
                            vars.push(variable.clone());
                        }
                    }
                }
            }
            LogicalOperator::Remove { items, .. } => {
                for item in items {
                    match item {
                        RemoveItem::Property { target } => {
                            collect_expression_variables(target, &mut vars);
                        }
                        RemoveItem::Label { variable, .. } => {
                            vars.push(variable.clone());
                        }
                    }
                }
            }
            LogicalOperator::Merge { pattern, .. } => {
                for elem in &pattern.elements {
                    if let PatternElement::Node(n) = elem {
                        if let Some(v) = &n.variable {
                            vars.push(v.clone());
                        }
                    }
                    if let PatternElement::Relationship(r) = elem {
                        if let Some(v) = &r.variable {
                            vars.push(v.clone());
                        }
                    }
                }
            }
            LogicalOperator::Apply { left, right } => {
                vars.extend(left.required_variables());
                vars.extend(right.required_variables());
            }
            LogicalOperator::HashJoin { left, right, join_keys } => {
                vars.extend(left.required_variables());
                vars.extend(right.required_variables());
                for k in join_keys {
                    vars.push(k.clone());
                }
            }
        }
        vars
    }
}

/// Recursively collect all variable names referenced in an expression.
fn collect_expression_variables(expr: &Expression, vars: &mut Vec<String>) {
    match expr {
        Expression::Variable(name) => {
            if !vars.contains(name) {
                vars.push(name.clone());
            }
        }
        Expression::PropertyAccess { base, .. } => {
            collect_expression_variables(base, vars);
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_expression_variables(left, vars);
            collect_expression_variables(right, vars);
        }
        Expression::Comparison { left, right, .. }
        | Expression::And { left, right, .. }
        | Expression::Or { left, right, .. }
        | Expression::Xor { left, right, .. }
        | Expression::StartsWith { left, right, .. }
        | Expression::EndsWith { left, right, .. }
        | Expression::Contains { left, right, .. }
        | Expression::In { left, right, .. }
        | Expression::Regex { left, right, .. } => {
            collect_expression_variables(left, vars);
            collect_expression_variables(right, vars);
        }
        Expression::UnaryOp { expr, .. } => {
            collect_expression_variables(expr, vars);
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => {
            collect_expression_variables(e, vars);
        }
        Expression::List(items) => {
            for item in items {
                collect_expression_variables(item, vars);
            }
        }
        Expression::Map(entries) => {
            for (_, v) in entries {
                collect_expression_variables(v, vars);
            }
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args {
                collect_expression_variables(arg, vars);
            }
        }
        Expression::Wildcard => {}
        Expression::Literal(_) => {}
    }
}

impl std::fmt::Display for AggregateFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AggregateFunction::Count => write!(f, "count"),
            AggregateFunction::Collect => write!(f, "collect"),
            AggregateFunction::Sum => write!(f, "sum"),
            AggregateFunction::Avg => write!(f, "avg"),
            AggregateFunction::Min => write!(f, "min"),
            AggregateFunction::Max => write!(f, "max"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{
        ComparisonOperator, Direction, Expression, Literal, NodePattern,
        Pattern, PatternElement, Projection,
    };
    use std::collections::HashMap;

    #[test]
    fn plan_explain_all_nodes_scan() {
        let plan = LogicalPlan::new(LogicalOperator::AllNodesScan);
        let explain = plan.explain();
        assert!(explain.contains("AllNodesScan"));
    }

    #[test]
    fn plan_explain_node_by_label() {
        let plan = LogicalPlan::new(LogicalOperator::NodeByLabelScan {
            label: "Person".to_string(),
        });
        let explain = plan.explain();
        assert!(explain.contains("NodeByLabelScan"));
        assert!(explain.contains("Person"));
    }

    #[test]
    fn plan_explain_expand() {
        let plan = LogicalPlan::new(LogicalOperator::Expand {
            input: Box::new(LogicalOperator::AllNodesScan),
            direction: Direction::Outgoing,
            rel_types: vec!["KNOWS".to_string()],
            rel_variable: Some("r".to_string()),
            end_node_variable: Some("m".to_string()),
            from_variable: "n".to_string(),
        });
        let explain = plan.explain();
        assert!(explain.contains("Expand"));
        assert!(explain.contains("KNOWS"));
    }

    #[test]
    fn plan_explain_filter() {
        let plan = LogicalPlan::new(LogicalOperator::Filter {
            input: Box::new(LogicalOperator::AllNodesScan),
            predicate: Expression::Comparison {
                span: None,
                op: ComparisonOperator::Gt,
                left: Box::new(Expression::Variable("age".to_string())),
                right: Box::new(Expression::Literal(Literal::Integer(18))),
            },
        });
        let explain = plan.explain();
        assert!(explain.contains("Filter"));
        assert!(explain.contains(">"));
    }

    #[test]
    fn plan_explain_project() {
        let plan = LogicalPlan::new(LogicalOperator::Project {
            input: Box::new(LogicalOperator::AllNodesScan),
            projections: vec![Projection { span: None,
                    expression: Expression::Variable("n".to_string()),
                alias: Some("node".to_string()),
            }],
        });
        let explain = plan.explain();
        assert!(explain.contains("Project"));
        assert!(explain.contains("node"));
    }

    #[test]
    fn plan_explain_nested_apply() {
        let left = LogicalOperator::AllNodesScan;
        let right = LogicalOperator::Filter {
            input: Box::new(LogicalOperator::AllNodesScan),
            predicate: Expression::Literal(Literal::Boolean(true)),
        };
        let plan = LogicalPlan::new(LogicalOperator::Apply {
            left: Box::new(left),
            right: Box::new(right),
        });
        let explain = plan.explain();
        assert!(explain.contains("Apply"));
        assert!(explain.contains("AllNodesScan"));
        assert!(explain.contains("Filter"));
    }

    #[test]
    fn expand_output_variables() {
        let op = LogicalOperator::Expand {
            input: Box::new(LogicalOperator::AllNodesScan),
            direction: Direction::Outgoing,
            rel_types: vec![],
            rel_variable: Some("r".to_string()),
            end_node_variable: Some("m".to_string()),
            from_variable: "n".to_string(),
        };
        let vars = op.output_variables();
        assert!(vars.contains(&"r".to_string()));
        assert!(vars.contains(&"m".to_string()));
    }

    #[test]
    fn filter_required_variables() {
        let op = LogicalOperator::Filter {
            input: Box::new(LogicalOperator::AllNodesScan),
            predicate: Expression::Variable("age".to_string()),
        };
        let vars = op.required_variables();
        assert!(vars.contains(&"age".to_string()));
    }

    #[test]
    fn project_output_variables_uses_alias() {
        let op = LogicalOperator::Project {
            input: Box::new(LogicalOperator::AllNodesScan),
            projections: vec![Projection { span: None,
                    expression: Expression::Variable("n".to_string()),
                alias: Some("node".to_string()),
            }],
        };
        let vars = op.output_variables();
        assert!(vars.contains(&"node".to_string()));
    }

    #[test]
    fn create_output_variables() {
        let pattern = Pattern {
            span: None,
            elements: vec![PatternElement::Node(NodePattern { span: None,
                variable: Some("n".to_string()),
                labels: vec!["Person".to_string()],
                properties: HashMap::new(),
            })],
        };
        let op = LogicalOperator::Create { input: None, pattern };
        let vars = op.output_variables();
        assert!(vars.contains(&"n".to_string()));
    }

    #[test]
    fn eager_is_transparent_to_variables() {
        let inner = LogicalOperator::Project {
            input: Box::new(LogicalOperator::AllNodesScan),
            projections: vec![Projection {
                span: None,
                expression: Expression::Variable("n".to_string()),
                alias: Some("node".to_string()),
            }],
        };
        let eager = LogicalOperator::Eager {
            input: Box::new(inner),
        };
        // Eager passes through exactly the variables of its input.
        assert!(eager.output_variables().contains(&"node".to_string()));
    }

    #[test]
    fn eager_explain_shows_barrier() {
        let plan = LogicalPlan::new(LogicalOperator::Eager {
            input: Box::new(LogicalOperator::AllNodesScan),
        });
        let explain = plan.explain();
        assert!(explain.contains("Eager"));
        assert!(explain.contains("AllNodesScan"));
    }
}
