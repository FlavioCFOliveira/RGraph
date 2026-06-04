//! Naive query executor for expression-only Cypher queries.
//!
//! This module provides the simplest end-to-end pipeline:
//!
//! ```text
//! query string → parser → semantic analyser → interpreter → result table
//! ```
//!
//! It is intentionally minimal: it only handles queries that consist of a
//! single `RETURN` clause (no `MATCH`, no `WHERE`, no graph state).  Its
//! purpose is to validate the full vertical slice from the CLI through the
//! parser to the execution engine before the more complex physical operators
//! are built.
//!
//! When the query contains clauses other than `RETURN`, the executor falls
//! back to the (future) physical engine.  For now it returns a clear
//! `Unsupported` error.

use crate::cypher::ast::{Clause, ReturnClause, Statement};
use crate::cypher::interpreter::{eval_projections, EvalContext, EvalError};
use crate::cypher::semantic::{analyse, SemanticError};
use crate::cypher::value::Value;

/// Result of executing a query.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Result rows.  Each row is a vector of values aligned with `columns`.
    pub rows: Vec<Vec<Value>>,
}

/// Execution error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// Semantic analysis failed.
    Semantic(String),
    /// Runtime evaluation failed.
    Eval(String),
    /// The query uses constructs not yet supported by the naive executor.
    Unsupported(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Semantic(msg) => write!(f, "semantic error: {}", msg),
            ExecError::Eval(msg) => write!(f, "evaluation error: {}", msg),
            ExecError::Unsupported(msg) => write!(f, "unsupported: {}", msg),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<SemanticError> for ExecError {
    fn from(e: SemanticError) -> Self {
        ExecError::Semantic(e.message)
    }
}

impl From<EvalError> for ExecError {
    fn from(e: EvalError) -> Self {
        ExecError::Eval(e.message)
    }
}

/// Execute a parsed [`Statement`] using the naive interpreter.
///
/// * If the statement contains only `RETURN`, evaluate each projection
///   against an empty [`EvalContext`] and produce a single-row result.
/// * If the statement contains `MATCH`, `WHERE`, `CREATE`, etc., return
///   [`ExecError::Unsupported`] so that the caller can route the query to
///   the physical execution engine once it is available.
///
/// The `analyse` pass is always run first so that semantic errors are caught
/// before any evaluation begins.
pub fn execute_naive(stmt: &Statement) -> Result<QueryResult, ExecError> {
    // Run semantic analysis even for RETURN-only queries so that we
    // exercise the full compile-time pipeline.
    let _scope = analyse(stmt)?;

    // Count non-RETURN clauses.
    let non_return: Vec<&Clause> = stmt
        .clauses
        .iter()
        .filter(|c| !matches!(c, Clause::Return(_)))
        .collect();

    if !non_return.is_empty() {
        let names: Vec<String> = non_return
            .iter()
            .map(|c| match c {
                Clause::Match(_) => "MATCH".to_string(),
                Clause::Where(_) => "WHERE".to_string(),
                Clause::Create(_) => "CREATE".to_string(),
                Clause::Delete(_) => "DELETE".to_string(),
                Clause::Set(_) => "SET".to_string(),
                Clause::Remove(_) => "REMOVE".to_string(),
                Clause::Merge(_) => "MERGE".to_string(),
                Clause::OptionalMatch(_) => "OPTIONAL MATCH".to_string(),
                Clause::With(_) => "WITH".to_string(),
                Clause::Unwind(_) => "UNWIND".to_string(),
                Clause::Union(_) => "UNION".to_string(),
                Clause::Call(_) => "CALL".to_string(),
                Clause::Foreach(_) => "FOREACH".to_string(),
                Clause::Return(_) => unreachable!(),
            })
            .collect();
        return Err(ExecError::Unsupported(format!(
            "naive executor does not support clauses: {}",
            names.join(", ")
        )));
    }

    // Extract the RETURN clause (there may be exactly one).
    let return_clause: &ReturnClause = stmt
        .clauses
        .iter()
        .find_map(|c| match c {
            Clause::Return(r) => Some(r),
            _ => None,
        })
        .ok_or_else(|| ExecError::Unsupported("no RETURN clause found".to_string()))?;

    let ctx = EvalContext::new();

    // Evaluate projections.
    let row_pairs = eval_projections(&return_clause.projections, &ctx)?;

    let columns: Vec<String> = row_pairs.iter().map(|(name, _)| name.clone()).collect();
    let values: Vec<Value> = row_pairs.into_iter().map(|(_, v)| v).collect();

    // Apply ORDER BY, SKIP, LIMIT (naive executor only supports a single
    // row, so these are mostly no-ops, but we validate the expressions).
    for item in &return_clause.order_by {
        // Validate that the ORDER BY expression can be evaluated.
        let _ = crate::cypher::interpreter::eval_order_key(&item.expression, &ctx)?;
    }
    if return_clause.skip.is_some() || return_clause.limit.is_some() {
        // For a single-row result, SKIP > 0 removes the row, LIMIT 0 removes
        // the row, but any other LIMIT is a no-op.
        let skip_val = return_clause
            .skip
            .as_ref()
            .map(|e| {
                crate::cypher::interpreter::evaluate(e, &ctx)
                    .and_then(|v| v.as_integer().ok_or_else(|| EvalError {
                        message: "SKIP value must be an integer".to_string(),
                    }))
            })
            .transpose()?;
        let limit_val = return_clause
            .limit
            .as_ref()
            .map(|e| {
                crate::cypher::interpreter::evaluate(e, &ctx)
                    .and_then(|v| v.as_integer().ok_or_else(|| EvalError {
                        message: "LIMIT value must be an integer".to_string(),
                    }))
            })
            .transpose()?;

        let mut rows = vec![values];

        if let Some(skip) = skip_val {
            if skip > 0 {
                rows.clear();
            }
        }
        if let Some(limit) = limit_val {
            if limit == 0 {
                rows.clear();
            } else if limit >= 1 {
                // no-op for single row
            }
        }

        return Ok(QueryResult { columns, rows });
    }

    Ok(QueryResult {
        columns,
        rows: vec![values],
    })
}

/// Convenience: parse, analyse, and execute a query string in one shot.
///
/// Returns `Err` for syntax errors, semantic errors, or unsupported clauses.
pub fn execute_expression_query(query: &str) -> Result<QueryResult, ExecError> {
    let stmt = crate::cypher::parser::parse(query).map_err(|e| ExecError::Eval(e.to_string()))?;
    execute_naive(&stmt)
}

// ------------------------------------------------------------------
// Display helpers
// ------------------------------------------------------------------

impl QueryResult {
    /// Render the result as a human-readable table.
    pub fn render_table(&self) -> String {
        if self.rows.is_empty() {
            return "(no results)".to_string();
        }
        let mut out = String::new();
        out.push_str(&self.columns.join(" | "));
        out.push('\n');
        for row in &self.rows {
            let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
        out
    }

    /// Render the result as a JSON-like array.
    pub fn render_json(&self) -> String {
        let rows: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (col, val) in self.columns.iter().zip(row.iter()) {
                    obj.insert(col.clone(), value_to_json(val));
                }
                serde_json::Value::Object(obj)
            })
            .collect();
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => {
            serde_json::Value::Number(serde_json::Number::from_f64(f.0).unwrap_or(0.into()))
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        Value::Map(entries) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in entries {
                obj.insert(k.clone(), value_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        Value::Node(n) => {
            let mut obj = serde_json::Map::new();
            obj.insert("id".to_string(), serde_json::Value::Number(n.id.into()));
            obj.insert("labels".to_string(), serde_json::Value::Array(
                n.labels.iter().map(|l| serde_json::Value::String(l.clone())).collect()
            ));
            serde_json::Value::Object(obj)
        }
        Value::Relationship(r) => {
            let mut obj = serde_json::Map::new();
            obj.insert("id".to_string(), serde_json::Value::Number(r.id.into()));
            obj.insert("type".to_string(), serde_json::Value::String(r.rel_type.clone()));
            serde_json::Value::Object(obj)
        }
        Value::Path(_) => serde_json::Value::String("<path>".to_string()),
        Value::Point { x, y, .. } => {
            let mut obj = serde_json::Map::new();
            obj.insert("x".to_string(), serde_json::Number::from_f64(*x)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null));
            obj.insert("y".to_string(), serde_json::Number::from_f64(*y)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null));
            serde_json::Value::Object(obj)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Clause, Expression, Literal, Projection, ReturnClause, Statement};

    #[test]
    fn execute_simple_return() {
        let stmt = Statement::new().with_clause(Clause::Return(ReturnClause {
            distinct: false,
            star: false,
            span: None,
            projections: vec![Projection {
                span: None,
                expression: Expression::Literal(Literal::Integer(42)),
                alias: None,
            }],
            order_by: vec![],
            skip: None,
            limit: None,
        }));
        let result = execute_naive(&stmt).unwrap();
        assert_eq!(result.columns, vec!["42"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0], vec![Value::Integer(42)]);
    }

    #[test]
    fn execute_return_with_alias() {
        let stmt = Statement::new().with_clause(Clause::Return(ReturnClause {
            distinct: false,
            star: false,
            span: None,
            projections: vec![Projection {
                span: None,
                expression: Expression::Literal(Literal::String("hello".to_string())),
                alias: Some("greeting".to_string()),
            }],
            order_by: vec![],
            skip: None,
            limit: None,
        }));
        let result = execute_naive(&stmt).unwrap();
        assert_eq!(result.columns, vec!["greeting"]);
        assert_eq!(result.rows[0][0], Value::String("hello".to_string()));
    }

    #[test]
    fn execute_return_expression() {
        let result = execute_expression_query("RETURN 1 + 2 * 3").unwrap();
        assert_eq!(result.rows[0][0], Value::Integer(7));
    }

    #[test]
    fn execute_return_null_semantics() {
        let result = execute_expression_query("RETURN NULL + 5").unwrap();
        assert_eq!(result.rows[0][0], Value::Null);
    }

    #[test]
    fn execute_return_list() {
        let result = execute_expression_query("RETURN [1, 2, 3]").unwrap();
        assert_eq!(
            result.rows[0][0],
            Value::List(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
            ])
        );
    }

    #[test]
    fn execute_return_map() {
        let result = execute_expression_query("RETURN {a: 1, b: 'two'}").unwrap();
        if let Value::Map(m) = &result.rows[0][0] {
            assert_eq!(m.get("a"), Some(&Value::Integer(1)));
            assert_eq!(
                m.get("b"),
                Some(&Value::String("two".to_string()))
            );
        } else {
            panic!("expected Map value");
        }
    }

    #[test]
    fn unsupported_match_clause() {
        let stmt = Statement::new()
            .with_clause(Clause::Match(crate::cypher::ast::MatchClause {
                span: None,
                patterns: vec![],
            }))
            .with_clause(Clause::Return(ReturnClause {
                distinct: false,
                star: false,
                span: None,
                projections: vec![Projection {
                    span: None,
                    expression: Expression::Literal(Literal::Integer(1)),
                    alias: None,
                }],
                order_by: vec![],
                skip: None,
                limit: None,
            }));
        let err = execute_naive(&stmt).unwrap_err();
        assert!(matches!(err, ExecError::Unsupported(_)));
        assert!(err.to_string().contains("MATCH"));
    }

    #[test]
    fn semantic_error_in_return() {
        let result = execute_expression_query("RETURN x + 1");
        assert!(result.is_err());
    }

    #[test]
    fn render_empty_result() {
        let qr = QueryResult {
            columns: vec!["a".to_string()],
            rows: vec![],
        };
        assert_eq!(qr.render_table(), "(no results)");
    }

    #[test]
    fn render_table() {
        let qr = QueryResult {
            columns: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec![Value::Integer(1), Value::String("x".to_string())]],
        };
        assert_eq!(qr.render_table(), "a | b\n1 | 'x'\n");
    }
}
