//! Semantic analyser for openCypher queries.
//!
//! Performs scope resolution and basic type checking over the AST produced
//! by the parser.  Detects undefined variables, duplicate bindings, and
//! clause-order violations before execution.

use crate::cypher::ast::*;
use crate::error::RGraphError;
use std::collections::{HashMap, HashSet};

/// Error produced by the semantic analysis pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticError {
    pub message: String,
}

impl std::fmt::Display for SemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "semantic error: {}", self.message)
    }
}

impl std::error::Error for SemanticError {}

impl From<SemanticError> for RGraphError {
    fn from(e: SemanticError) -> Self {
        RGraphError::Semantic(e.message)
    }
}

/// Semantic state accumulated while walking the AST.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Variables introduced by MATCH patterns.
    pub variables: HashSet<String>,
    /// Track which clause introduced each variable (for error messages).
    pub variable_source: HashMap<String, String>,
}

/// Analyse a complete statement and return the populated scope or an error.
pub fn analyse(stmt: &Statement) -> Result<Scope, SemanticError> {
    let mut scope = Scope::default();
    let mut has_scope = false;

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(m) => {
                has_scope = true;
                collect_pattern_variables(&m.pattern, &mut scope)?;
            }
            Clause::Where(w) => {
                if !has_scope {
                    return Err(SemanticError {
                        message: "WHERE clause must follow MATCH or CREATE".to_string(),
                    });
                }
                check_expression_variables(&w.predicate, &scope)?;
            }
            Clause::Return(r) => {
                // RETURN-only queries (e.g. "RETURN 1+2") are valid in openCypher.
                // They operate on an empty scope, so any variable references will
                // be caught by check_expression_variables.
                has_scope = true;
                for proj in &r.projections {
                    check_expression_variables(&proj.expression, &scope)?;
                    if let Some(alias) = &proj.alias {
                        scope.variables.insert(alias.clone());
                    }
                }
                for item in &r.order_by {
                    check_expression_variables(&item.expression, &scope)?;
                }
            }
            Clause::Create(c) => {
                // CREATE introduces variables into scope for subsequent clauses.
                has_scope = true;
                collect_pattern_variables(&c.pattern, &mut scope)?;
            }
        }
    }

    Ok(scope)
}

/// Extract all variable names introduced by a pattern and add them to scope.
fn collect_pattern_variables(
    pattern: &Pattern,
    scope: &mut Scope,
) -> Result<(), SemanticError> {
    for elem in &pattern.elements {
        match elem {
            PatternElement::Node(n) => {
                if let Some(v) = &n.variable {
                    if scope.variables.contains(v) {
                        return Err(SemanticError {
                            message: format!(
                                "variable '{}' already bound in this scope",
                                v
                            ),
                        });
                    }
                    scope.variables.insert(v.clone());
                    scope.variable_source.insert(v.clone(), "MATCH".to_string());
                }
            }
            PatternElement::Relationship(r) => {
                if let Some(v) = &r.variable {
                    if scope.variables.contains(v) {
                        return Err(SemanticError {
                            message: format!(
                                "variable '{}' already bound in this scope",
                                v
                            ),
                        });
                    }
                    scope.variables.insert(v.clone());
                    scope.variable_source.insert(v.clone(), "MATCH".to_string());
                }
            }
        }
    }
    Ok(())
}

/// Ensure every variable referenced in `expr` is present in `scope`.
fn check_expression_variables(
    expr: &Expression,
    scope: &Scope,
) -> Result<(), SemanticError> {
    match expr {
        Expression::Variable(name) => {
            if !scope.variables.contains(name) {
                return Err(SemanticError {
                    message: format!("undefined variable '{}'", name),
                });
            }
        }
        Expression::PropertyAccess { base, .. } => {
            check_expression_variables(base, scope)?;
        }
        Expression::BinaryOp { left, right, .. } => {
            check_expression_variables(left, scope)?;
            check_expression_variables(right, scope)?;
        }
        Expression::Comparison { left, right, .. } => {
            check_expression_variables(left, scope)?;
            check_expression_variables(right, scope)?;
        }
        Expression::UnaryOp { expr, .. } => {
            check_expression_variables(expr, scope)?;
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => {
            check_expression_variables(e, scope)?;
        }
        Expression::List(items) => {
            for item in items {
                check_expression_variables(item, scope)?;
            }
        }
        Expression::Map(entries) => {
            for (_, v) in entries {
                check_expression_variables(v, scope)?;
            }
        }
        Expression::Literal(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;

    #[test]
    fn analyse_valid_match_where_return() {
        let stmt = parse("MATCH (n:Person)-[:KNOWS]->(m:Person) WHERE n.age > 18 RETURN n, m").unwrap();
        let scope = analyse(&stmt).unwrap();
        assert!(scope.variables.contains("n"));
        assert!(scope.variables.contains("m"));
    }

    #[test]
    fn analyse_undefined_variable_in_where() {
        let stmt = parse("MATCH (n:Person) WHERE x.age > 18 RETURN n").unwrap();
        let err = analyse(&stmt).unwrap_err();
        assert!(err.message.contains("undefined variable 'x'"));
    }

    #[test]
    fn analyse_undefined_variable_in_return() {
        let stmt = parse("MATCH (n:Person) RETURN x").unwrap();
        let err = analyse(&stmt).unwrap_err();
        assert!(err.message.contains("undefined variable 'x'"));
    }

    #[test]
    fn analyse_duplicate_variable() {
        let stmt = parse("MATCH (n:Person)-[]->(n:Person) RETURN n").unwrap();
        let err = analyse(&stmt).unwrap_err();
        assert!(err.message.contains("already bound"));
    }

    #[test]
    fn analyse_where_without_match() {
        let stmt = parse("WHERE n.age > 18 RETURN n").unwrap();
        let err = analyse(&stmt).unwrap_err();
        assert!(err.message.contains("WHERE clause must follow MATCH or CREATE"));
    }

    #[test]
    fn analyse_return_without_match_is_valid() {
        // RETURN-only queries (e.g. "RETURN 42") are valid openCypher.
        let stmt = parse("RETURN 42").unwrap();
        let scope = analyse(&stmt).unwrap();
        // No variables introduced, no errors.
        assert!(scope.variables.is_empty());
    }

    #[test]
    fn analyse_create_introduces_variables() {
        let stmt = parse("CREATE (n:Person {name: 'Alice'}) RETURN n").unwrap();
        let scope = analyse(&stmt).unwrap();
        assert!(scope.variables.contains("n"));
    }
}
