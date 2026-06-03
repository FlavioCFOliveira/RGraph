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
        RGraphError::Semantic(e.message)
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
pub fn plan(stmt: &Statement) -> Result<LogicalPlan, PlanError> {
    let mut current: Option<LogicalOperator> = None;

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(m) => {
                current = Some(build_match_plan(&m.pattern, current)?);
            }
            Clause::Where(w) => {
                let input = current.unwrap_or(LogicalOperator::AllNodesScan);
                current = Some(LogicalOperator::Filter {
                    input: Box::new(input),
                    predicate: w.predicate.clone(),
                });
            }
            Clause::Create(c) => {
                // CREATE is a standalone write operator.  If there is already
                // a match plan, we stack Create on top (rare in openCypher but
                // supported by the grammar).
                let input = current;
                current = Some(LogicalOperator::Create {
                    pattern: c.pattern.clone(),
                });
                // Preserve input chain if present by wrapping in Apply
                // (stub — physical engine will handle this properly).
                if let Some(inp) = input {
                    current = Some(LogicalOperator::Apply {
                        left: Box::new(inp),
                        right: Box::new(current.unwrap()),
                    });
                }
            }
            Clause::Return(r) => {
                current = Some(build_return_plan(r, current)?);
            }
        }
    }

    let root = current.unwrap_or(LogicalOperator::AllNodesScan);
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
    let mut elements = pattern.elements.iter().peekable();
    let mut current_op: Option<LogicalOperator> = input;

    while let Some(elem) = elements.next() {
        match elem {
            PatternElement::Node(node) => {
                let scan = if let Some(label) = node.labels.first() {
                    LogicalOperator::NodeByLabelScan {
                        label: label.clone(),
                    }
                } else {
                    LogicalOperator::AllNodesScan
                };
                current_op = Some(scan);
            }
            PatternElement::Relationship(rel) => {
                // We need the previous node variable as the start point.
                let from_var = find_previous_node_variable(&pattern, &elements)
                    .unwrap_or_else(|| "_".to_string());

                let expand = LogicalOperator::Expand {
                    input: Box::new(current_op.unwrap_or(LogicalOperator::AllNodesScan)),
                    direction: rel.direction,
                    rel_types: rel.types.clone(),
                    rel_variable: rel.variable.clone(),
                    end_node_variable: find_next_node_variable(&mut elements),
                    from_variable: from_var,
                };
                current_op = Some(expand);
            }
        }
    }

    current_op.ok_or_else(|| PlanError {
        message: "empty MATCH pattern".to_string(),
    })
}

fn find_previous_node_variable<'a>(
    pattern: &Pattern,
    _elements: &std::iter::Peekable<std::slice::Iter<'a, PatternElement>>,
) -> Option<String> {
    // In a well-formed pattern the node before the relationship is the
    // one we just processed.  We scan backwards from the current position.
    // For simplicity, we return the first node variable we find.
    for elem in &pattern.elements {
        if let PatternElement::Node(node) = elem {
            if let Some(v) = &node.variable {
                return Some(v.clone());
            }
        }
    }
    None
}

fn find_next_node_variable<'a>(
    elements: &mut std::iter::Peekable<std::slice::Iter<'a, PatternElement>>,
) -> Option<String> {
    if let Some(PatternElement::Node(node)) = elements.peek() {
        node.variable.clone()
    } else {
        None
    }
}

// ------------------------------------------------------------------
// Return planning
// ------------------------------------------------------------------

/// Build the operator tree for a `RETURN` clause.
///
/// The tree is stacked as:
/// ```text
/// Project
///   Sort (optional)
///     Skip (optional)
///       Limit (optional)
///         input
/// ```
fn build_return_plan(
    ret: &ReturnClause,
    input: Option<LogicalOperator>,
) -> Result<LogicalOperator, PlanError> {
    let mut op = input.unwrap_or(LogicalOperator::AllNodesScan);

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

    op = LogicalOperator::Project {
        input: Box::new(op),
        projections: ret.projections.clone(),
    };

    Ok(op)
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
}
