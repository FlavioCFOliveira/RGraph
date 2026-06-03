//! Expression interpreter — evaluates [`ast::Expression`] into [`Value`].
//!
//! The interpreter is the simplest possible execution layer: it walks the AST
//! directly without any query planning or I/O.  It is intended for
//! expression-only queries (`RETURN 1+2`) and as a building block for the
//! full physical execution engine.

use crate::cypher::ast::*;
use crate::cypher::value::Value;
use crate::graph::property::OrderedF64;
use std::collections::HashMap;

/// Runtime error produced during expression evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
    pub message: String,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "evaluation error: {}", self.message)
    }
}

impl std::error::Error for EvalError {}

/// Evaluation context — maps variable names to runtime values.
///
/// For expression-only queries the context is typically empty.  When the
/// interpreter is used inside a physical operator (e.g.  `Project`) the
/// context is populated from the current input row.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    bindings: HashMap<String, Value>,
}

impl EvalContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a variable name to a value.
    pub fn bind(mut self, name: impl Into<String>, value: Value) -> Self {
        self.bindings.insert(name.into(), value);
        self
    }

    /// Look up a variable by name.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.bindings.get(name)
    }

    /// Return all bindings as an owned map.
    pub fn into_bindings(self) -> HashMap<String, Value> {
        self.bindings
    }
}

/// Evaluate an AST [`Expression`] in the given [`EvalContext`].
///
/// Returns [`EvalError`] for type mismatches or undefined variables.
/// `Null` propagation is handled by the [`Value`] arithmetic methods.
pub fn evaluate(expr: &Expression, ctx: &EvalContext) -> Result<Value, EvalError> {
    match expr {
        Expression::Literal(lit) => Ok(eval_literal(lit)),

        Expression::Variable(name) => ctx
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError {
                message: format!("undefined variable '{}'", name),
            }),

        Expression::PropertyAccess { base, property, .. } => {
            let base_val = evaluate(base, ctx)?;
            match base_val {
                Value::Map(mut entries) => entries
                    .remove(property)
                    .ok_or_else(|| EvalError {
                        message: format!("property '{}' not found", property),
                    }),
                Value::Null => Ok(Value::Null),
                _ => Err(EvalError {
                    message: format!(
                        "cannot access property '{}' on value of type '{}'",
                        property,
                        base_val.type_name()
                    ),
                }),
            }
        }

        Expression::BinaryOp { op, left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            let result = match op {
                BinaryOperator::Add => l.add(&r),
                BinaryOperator::Sub => l.sub(&r),
                BinaryOperator::Mul => l.mul(&r),
                BinaryOperator::Div => l.div(&r),
                BinaryOperator::Mod => l.modulo(&r),
                BinaryOperator::Pow => l.pow(&r),
            };
            result.ok_or_else(|| EvalError {
                message: format!(
                    "type mismatch: cannot apply {:?} to '{}' and '{}'",
                    op,
                    l.type_name(),
                    r.type_name()
                ),
            })
        }

        Expression::Comparison { op, left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            let result = match op {
                ComparisonOperator::Eq => l.eq(&r),
                ComparisonOperator::Ne => l.ne(&r),
                ComparisonOperator::Lt => l.lt(&r),
                ComparisonOperator::Le => l.le(&r),
                ComparisonOperator::Gt => l.gt(&r),
                ComparisonOperator::Ge => l.ge(&r),
            };
            result.ok_or_else(|| EvalError {
                message: format!(
                    "type mismatch: cannot compare '{}' and '{}' with {:?}",
                    l.type_name(),
                    r.type_name(),
                    op
                ),
            })
        }

        Expression::UnaryOp { op, expr, .. } => {
            let v = evaluate(expr, ctx)?;
            match op {
                UnaryOperator::Not => v.not().ok_or_else(|| EvalError {
                    message: format!("expected Boolean, got '{}'", v.type_name()),
                }),
                UnaryOperator::Neg => v.negate().ok_or_else(|| EvalError {
                    message: format!(
                        "expected numeric type, got '{}'",
                        v.type_name()
                    ),
                }),
            }
        }

        Expression::IsNull(e) => {
            let v = evaluate(e, ctx)?;
            Ok(v.is_null_predicate())
        }

        Expression::IsNotNull(e) => {
            let v = evaluate(e, ctx)?;
            Ok(v.is_not_null_predicate())
        }

        Expression::And { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::Boolean(a), Value::Boolean(b)) => Ok(Value::Boolean(a && b)),
                (a, b) => Err(EvalError {
                    message: format!("expected Boolean, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::Or { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::Boolean(a), Value::Boolean(b)) => Ok(Value::Boolean(a || b)),
                (a, b) => Err(EvalError {
                    message: format!("expected Boolean, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::Xor { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::Boolean(a), Value::Boolean(b)) => Ok(Value::Boolean(a ^ b)),
                (a, b) => Err(EvalError {
                    message: format!("expected Boolean, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::StartsWith { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.starts_with(&b))),
                (a, b) => Err(EvalError {
                    message: format!("expected String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::EndsWith { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.ends_with(&b))),
                (a, b) => Err(EvalError {
                    message: format!("expected String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::Contains { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.contains(&b))),
                (a, b) => Err(EvalError {
                    message: format!("expected String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::In { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match r {
                Value::List(items) => Ok(Value::Boolean(items.contains(&l))),
                _ => Err(EvalError {
                    message: format!("expected List, got '{}'", r.type_name()),
                }),
            }
        }

        Expression::Regex { left, right, .. } => {
            let _l = evaluate(left, ctx)?;
            let _r = evaluate(right, ctx)?;
            // TODO: implement regex matching (requires regex crate integration)
            Err(EvalError {
                message: "regex matching (=~) not yet implemented".to_string(),
            })
        }

        Expression::List(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(evaluate(item, ctx)?);
            }
            Ok(Value::List(values))
        }

        Expression::Map(entries) => {
            let mut map = HashMap::with_capacity(entries.len());
            for (k, v_expr) in entries {
                map.insert(k.clone(), evaluate(v_expr, ctx)?);
            }
            Ok(Value::Map(map))
        }
        Expression::Wildcard => {
            Err(EvalError {
                message: "wildcard * cannot be evaluated in expression context".to_string(),
            })
        }
        Expression::FunctionCall { name, .. } => {
            // For Sprint 21, aggregate functions are evaluated in the
            // AggregateOp physical operator, not here.  Scalar functions
            // (e.g. toString, size) are not yet supported.
            Err(EvalError {
                message: format!("function '{}' not supported in expression evaluator", name),
            })
        }
    }
}

fn eval_literal(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Integer(v) => Value::Integer(*v),
        Literal::Float(v) => Value::Float(OrderedF64(*v)),
        Literal::String(s) => Value::String(s.clone()),
    }
}

// ------------------------------------------------------------------
// Helpers for the full query executor
// ------------------------------------------------------------------

/// Evaluate a list of projections and return a row (ordered map of alias → value).
///
/// When an alias is missing, the expression itself is used as the column name
/// (e.g. `n.name` becomes the string `"n.name"`).
pub fn eval_projections(
    projections: &[Projection],
    ctx: &EvalContext,
) -> Result<Vec<(String, Value)>, EvalError> {
    let mut row = Vec::with_capacity(projections.len());
    for proj in projections {
        let value = evaluate(&proj.expression, ctx)?;
        let name = proj
            .alias
            .clone()
            .unwrap_or_else(|| proj.expression.to_string());
        row.push((name, value));
    }
    Ok(row)
}

/// Evaluate an ORDER BY expression and return a sortable scalar value.
///
/// Lists, maps, and `Null` are returned as-is; callers are responsible for
/// comparison semantics.
pub fn eval_order_key(expr: &Expression, ctx: &EvalContext) -> Result<Value, EvalError> {
    evaluate(expr, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{BinaryOperator, ComparisonOperator, Expression, Literal, Projection, UnaryOperator};

    #[test]
    fn eval_literal_integer() {
        let ctx = EvalContext::new();
        let expr = Expression::Literal(Literal::Integer(42));
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Integer(42));
    }

    #[test]
    fn eval_arithmetic_addition() {
        let ctx = EvalContext::new();
        let expr = Expression::BinaryOp {
            span: None,
            op: BinaryOperator::Add,
            left: Box::new(Expression::Literal(Literal::Integer(3))),
            right: Box::new(Expression::Literal(Literal::Integer(4))),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Integer(7));
    }

    #[test]
    fn eval_variable_lookup() {
        let ctx = EvalContext::new().bind("x", Value::Integer(99));
        let expr = Expression::Variable("x".to_string());
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Integer(99));
    }

    #[test]
    fn eval_undefined_variable_errors() {
        let ctx = EvalContext::new();
        let expr = Expression::Variable("y".to_string());
        assert!(evaluate(&expr, &ctx).is_err());
    }

    #[test]
    fn eval_comparison() {
        let ctx = EvalContext::new();
        let expr = Expression::Comparison {
            span: None,
            op: ComparisonOperator::Gt,
            left: Box::new(Expression::Literal(Literal::Integer(5))),
            right: Box::new(Expression::Literal(Literal::Integer(3))),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Boolean(true));
    }

    #[test]
    fn eval_not_operator() {
        let ctx = EvalContext::new();
        let expr = Expression::UnaryOp {
            span: None,
            op: UnaryOperator::Not,
            expr: Box::new(Expression::Literal(Literal::Boolean(true))),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Boolean(false));
    }

    #[test]
    fn eval_neg_operator() {
        let ctx = EvalContext::new();
        let expr = Expression::UnaryOp {
            span: None,
            op: UnaryOperator::Neg,
            expr: Box::new(Expression::Literal(Literal::Integer(7))),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Integer(-7));
    }

    #[test]
    fn eval_is_null() {
        let ctx = EvalContext::new();
        let expr = Expression::IsNull(Box::new(Expression::Literal(Literal::Null)));
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Boolean(true));
    }

    #[test]
    fn eval_list_literal() {
        let ctx = EvalContext::new();
        let expr = Expression::List(vec![
            Expression::Literal(Literal::Integer(1)),
            Expression::Literal(Literal::Integer(2)),
        ]);
        assert_eq!(
            evaluate(&expr, &ctx).unwrap(),
            Value::List(vec![Value::Integer(1), Value::Integer(2)])
        );
    }

    #[test]
    fn eval_map_literal() {
        let ctx = EvalContext::new();
        let expr = Expression::Map(vec![(
            "key".to_string(),
            Expression::Literal(Literal::String("val".to_string())),
        )]);
        let mut expected = HashMap::new();
        expected.insert("key".to_string(), Value::String("val".to_string()));
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Map(expected));
    }

    #[test]
    fn eval_property_access() {
        let mut map = HashMap::new();
        map.insert("name".to_string(), Value::String("Alice".to_string()));
        let ctx = EvalContext::new().bind("n", Value::Map(map));
        let expr = Expression::PropertyAccess {
            span: None,
            base: Box::new(Expression::Variable("n".to_string())),
            property: "name".to_string(),
        };
        assert_eq!(
            evaluate(&expr, &ctx).unwrap(),
            Value::String("Alice".to_string())
        );
    }

    #[test]
    fn eval_property_access_on_null() {
        let ctx = EvalContext::new().bind("n", Value::Null);
        let expr = Expression::PropertyAccess {
            span: None,
            base: Box::new(Expression::Variable("n".to_string())),
            property: "name".to_string(),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Null);
    }

    #[test]
    fn eval_projections_with_alias() {
        let ctx = EvalContext::new().bind("x", Value::Integer(10));
        let projs = vec![
            Projection {
            span: None,
            expression: Expression::Variable("x".to_string()),
                alias: Some("a".to_string()),
            },
            Projection {
            span: None,
            expression: Expression::Literal(Literal::Integer(5)),
                alias: None,
            },
        ];
        let row = eval_projections(&projs, &ctx).unwrap();
        assert_eq!(row[0], ("a".to_string(), Value::Integer(10)));
        assert_eq!(row[1], ("5".to_string(), Value::Integer(5)));
    }

    #[test]
    fn eval_null_arithmetic() {
        let ctx = EvalContext::new();
        let expr = Expression::BinaryOp {
            span: None,
            op: BinaryOperator::Add,
            left: Box::new(Expression::Literal(Literal::Null)),
            right: Box::new(Expression::Literal(Literal::Integer(5))),
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Null);
    }

    #[test]
    fn eval_string_concatenation() {
        let ctx = EvalContext::new();
        let expr = Expression::BinaryOp {
            span: None,
            op: BinaryOperator::Add,
            left: Box::new(Expression::Literal(Literal::String("hello".to_string()))),
            right: Box::new(Expression::Literal(Literal::String(" ".to_string()))),
        };
        let result = evaluate(&expr, &ctx).unwrap();
        assert_eq!(result, Value::String("hello ".to_string()));
    }
}
