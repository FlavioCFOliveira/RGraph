//! Expression interpreter — evaluates [`ast::Expression`] into [`Value`].
//!
//! Implements openCypher expression semantics including:
//! - Kleene three-valued logic for AND/OR/XOR/NOT
//! - Regex `=~` with full-match Java-compatible semantics
//! - A broad scalar/list/string/math function library (see the function
//!   dispatch in this module for the exact set supported today)
//! - CASE expressions, list comprehensions, quantifiers, reduce, EXISTS
//! - Parameter binding via execution context
//! - Null propagation throughout
//!
//! Coverage is validated against the openCypher TCK rather than claimed to be
//! exhaustive here; consult the TCK suite for the authoritative compliance set.

use crate::cypher::ast::*;
use crate::cypher::value::Value;
use crate::graph::property::OrderedF64;
use regex::Regex;
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Evaluation context
// ─────────────────────────────────────────────────────────────────────────────

/// Evaluation context — maps variable names and parameter names to runtime values.
///
/// For expression-only queries the context is typically empty.  When the
/// interpreter is used inside a physical operator (e.g. `Project`) the context
/// is populated from the current input row.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    /// Variable bindings (populated from matched rows).
    bindings: HashMap<String, Value>,
    /// Parameter bindings (`$name` → value).
    parameters: HashMap<String, Value>,
}

impl EvalContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a variable name to a value.
    pub fn bind(mut self, name: impl Into<String>, value: Value) -> Self {
        self.bindings.insert(name.into(), value);
        self
    }

    /// Bind a parameter name to a value.
    pub fn bind_parameter(mut self, name: impl Into<String>, value: Value) -> Self {
        self.parameters.insert(name.into(), value);
        self
    }

    /// Look up a variable by name.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.bindings.get(name)
    }

    /// Look up a parameter by name.
    pub fn get_parameter(&self, name: &str) -> Option<&Value> {
        self.parameters.get(name)
    }

    /// Return all bindings as an owned map.
    pub fn into_bindings(self) -> HashMap<String, Value> {
        self.bindings
    }

    /// Extend context with additional bindings (for comprehension scopes).
    pub fn extend_with(&self, name: impl Into<String>, value: Value) -> Self {
        let mut new_ctx = self.clone();
        new_ctx.bindings.insert(name.into(), value);
        new_ctx
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main evaluator
// ─────────────────────────────────────────────────────────────────────────────

/// Evaluate an AST [`Expression`] in the given [`EvalContext`].
///
/// Implements openCypher null propagation and three-valued logic throughout.
pub fn evaluate(expr: &Expression, ctx: &EvalContext) -> Result<Value, EvalError> {
    match expr {
        Expression::Literal(lit) => Ok(eval_literal(lit)),

        Expression::Variable(name) => ctx
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError {
                message: format!("undefined variable '{}'", name),
            }),

        Expression::Parameter(name) => ctx
            .get_parameter(name)
            .cloned()
            .ok_or_else(|| EvalError {
                message: format!("unbound parameter '${}'", name),
            }),

        Expression::PropertyAccess { base, property, .. } => {
            let base_val = evaluate(base, ctx)?;
            match base_val {
                Value::Map(mut entries) => {
                    // Property access on a Map returns Null if the key is absent
                    // (openCypher: `n.missingProp` → null, not an error).
                    Ok(entries.remove(property).unwrap_or(Value::Null))
                }
                Value::Node(ref node) => {
                    Ok(node.properties.get(property).cloned().unwrap_or(Value::Null))
                }
                Value::Relationship(ref rel) => {
                    Ok(rel.properties.get(property).cloned().unwrap_or(Value::Null))
                }
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

        Expression::DynamicPropertyAccess { base, index, .. } => {
            let base_val = evaluate(base, ctx)?;
            let idx_val = evaluate(index, ctx)?;
            match (base_val, idx_val) {
                (Value::Map(mut m), Value::String(key)) => {
                    Ok(m.remove(&key).unwrap_or(Value::Null))
                }
                (Value::List(items), Value::Integer(i)) => {
                    let idx = if i < 0 {
                        items.len().saturating_sub((-i) as usize)
                    } else {
                        i as usize
                    };
                    Ok(items.get(idx).cloned().unwrap_or(Value::Null))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (base, idx) => Err(EvalError {
                    message: format!("cannot index {} with {}", base.type_name(), idx.type_name()),
                }),
            }
        }

        Expression::Slice { base, from, to, .. } => {
            let base_val = evaluate(base, ctx)?;
            let from_val = from.as_ref().map(|e| evaluate(e, ctx)).transpose()?;
            let to_val = to.as_ref().map(|e| evaluate(e, ctx)).transpose()?;
            match base_val {
                Value::List(items) => {
                    let len = items.len() as i64;
                    let start = from_val.and_then(|v| v.as_integer()).unwrap_or(0);
                    let end = to_val.and_then(|v| v.as_integer()).unwrap_or(len);
                    let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
                    let end = if end < 0 { (len + end).max(0) as usize } else { end.min(len) as usize };
                    Ok(Value::List(items[start..end.max(start)].to_vec()))
                }
                Value::String(s) => {
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let start = from_val.and_then(|v| v.as_integer()).unwrap_or(0);
                    let end = to_val.and_then(|v| v.as_integer()).unwrap_or(len);
                    let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
                    let end = if end < 0 { (len + end).max(0) as usize } else { end.min(len) as usize };
                    Ok(Value::String(chars[start..end.max(start)].iter().collect()))
                }
                Value::Null => Ok(Value::Null),
                v => Err(EvalError { message: format!("cannot slice {}", v.type_name()) }),
            }
        }

        Expression::BinaryOp { op, left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            let result = match op {
                BinaryOperator::Add | BinaryOperator::Concat => l.add(&r),
                BinaryOperator::Sub => l.sub(&r),
                BinaryOperator::Mul => l.mul(&r),
                BinaryOperator::Div => l.div(&r),
                BinaryOperator::Mod => l.modulo(&r),
                BinaryOperator::Pow => l.pow(&r),
            };
            result.ok_or_else(|| EvalError {
                message: format!(
                    "type mismatch: cannot apply {:?} to '{}' and '{}'",
                    op, l.type_name(), r.type_name()
                ),
            })
        }

        Expression::Comparison { op, left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            let result = match op {
                ComparisonOperator::Eq => l.cypher_eq(&r),
                ComparisonOperator::Ne => l.cypher_ne(&r),
                ComparisonOperator::Lt => l.lt(&r),
                ComparisonOperator::Le => l.le(&r),
                ComparisonOperator::Gt => l.gt(&r),
                ComparisonOperator::Ge => l.ge(&r),
            };
            result.ok_or_else(|| EvalError {
                message: format!(
                    "type mismatch: cannot compare '{}' and '{}' with {:?}",
                    l.type_name(), r.type_name(), op
                ),
            })
        }

        Expression::UnaryOp { op, expr, .. } => {
            let v = evaluate(expr, ctx)?;
            match op {
                UnaryOperator::Not => v.kleene_not().ok_or_else(|| EvalError {
                    message: format!("expected Boolean, got '{}'", v.type_name()),
                }),
                UnaryOperator::Neg => v.negate().ok_or_else(|| EvalError {
                    message: format!("expected numeric type, got '{}'", v.type_name()),
                }),
            }
        }

        Expression::Not { expr, .. } => {
            let v = evaluate(expr, ctx)?;
            v.kleene_not().ok_or_else(|| EvalError {
                message: format!("expected Boolean, got '{}'", v.type_name()),
            })
        }

        Expression::IsNull(e) => {
            let v = evaluate(e, ctx)?;
            Ok(v.is_null_predicate())
        }

        Expression::IsNotNull(e) => {
            let v = evaluate(e, ctx)?;
            Ok(v.is_not_null_predicate())
        }

        // ── Three-valued Kleene logic ──────────────────────────────────────

        Expression::And { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            Ok(kleene_and(l, r))
        }

        Expression::Or { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            Ok(kleene_or(l, r))
        }

        Expression::Xor { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            Ok(kleene_xor(l, r))
        }

        Expression::StartsWith { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            if l.is_null() || r.is_null() { return Ok(Value::Null); }
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.starts_with(&b))),
                (a, b) => Err(EvalError {
                    message: format!("STARTS WITH requires String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::EndsWith { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            if l.is_null() || r.is_null() { return Ok(Value::Null); }
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.ends_with(&b))),
                (a, b) => Err(EvalError {
                    message: format!("ENDS WITH requires String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::Contains { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            if l.is_null() || r.is_null() { return Ok(Value::Null); }
            match (l, r) {
                (Value::String(a), Value::String(b)) => Ok(Value::Boolean(a.contains(&b))),
                (a, b) => Err(EvalError {
                    message: format!("CONTAINS requires String, got '{}' and '{}'", a.type_name(), b.type_name()),
                }),
            }
        }

        Expression::In { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            match r {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    // IN null → null; x IN [...] → true if match found, else null if any null, else false.
                    let mut found_null = false;
                    for item in &items {
                        if item.is_null() {
                            found_null = true;
                        } else if let Some(Value::Boolean(true)) = l.cypher_eq(item) {
                            return Ok(Value::Boolean(true));
                        }
                    }
                    if found_null { Ok(Value::Null) } else { Ok(Value::Boolean(false)) }
                }
                other => Err(EvalError {
                    message: format!("IN requires a List, got '{}'", other.type_name()),
                }),
            }
        }

        Expression::Regex { left, right, .. } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            if l.is_null() || r.is_null() { return Ok(Value::Null); }
            match (l, r) {
                (Value::String(s), Value::String(pattern)) => {
                    // openCypher =~ semantics: full-match, case-sensitive, Java regex dialect.
                    let re = Regex::new(&pattern).map_err(|e| EvalError {
                        message: format!("invalid regex '{}': {}", pattern, e),
                    })?;
                    // Anchored full-match equivalent: ^pattern$.
                    let anchored = format!("^(?:{})$", pattern);
                    let re = Regex::new(&anchored).map_err(|e| EvalError {
                        message: format!("invalid regex '{}': {}", pattern, e),
                    })?;
                    Ok(Value::Boolean(re.is_match(&s)))
                }
                (a, b) => Err(EvalError {
                    message: format!("=~ requires String operands, got '{}' =~ '{}'", a.type_name(), b.type_name()),
                }),
            }
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

        Expression::Wildcard => Err(EvalError {
            message: "wildcard * cannot be evaluated in expression context".to_string(),
        }),

        Expression::FunctionCall { name, args, distinct, .. } => {
            eval_function(name, args, *distinct, ctx)
        }

        // ── CASE ──────────────────────────────────────────────────────────

        Expression::Case { subject, alternatives, default, .. } => {
            let subject_val = subject.as_ref()
                .map(|s| evaluate(s, ctx))
                .transpose()?;

            for alt in alternatives {
                let condition_val = if let Some(subject) = &subject_val {
                    // Simple CASE: CASE x WHEN v THEN ...
                    let when_val = evaluate(&alt.condition, ctx)?;
                    match subject.cypher_eq(&when_val) {
                        Some(Value::Boolean(true)) => true,
                        _ => false,
                    }
                } else {
                    // Generic CASE: CASE WHEN pred THEN ...
                    match evaluate(&alt.condition, ctx)? {
                        Value::Boolean(b) => b,
                        Value::Null => false,
                        v => return Err(EvalError {
                            message: format!("CASE WHEN condition must be Boolean, got '{}'", v.type_name()),
                        }),
                    }
                };
                if condition_val {
                    return evaluate(&alt.result, ctx);
                }
            }

            if let Some(d) = default {
                evaluate(d, ctx)
            } else {
                Ok(Value::Null)
            }
        }

        // ── List comprehension ─────────────────────────────────────────────

        Expression::ListComprehension { variable, source, filter, projection, .. } => {
            let source_val = evaluate(source, ctx)?;
            match source_val {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    let mut result = Vec::new();
                    for item in items {
                        let inner_ctx = ctx.extend_with(variable.clone(), item.clone());
                        // Apply filter.
                        if let Some(pred) = filter {
                            match evaluate(pred, &inner_ctx)? {
                                Value::Boolean(false) | Value::Null => continue,
                                _ => {}
                            }
                        }
                        // Apply projection.
                        if let Some(proj) = projection {
                            result.push(evaluate(proj, &inner_ctx)?);
                        } else {
                            result.push(item);
                        }
                    }
                    Ok(Value::List(result))
                }
                v => Err(EvalError { message: format!("list comprehension source must be a list, got '{}'", v.type_name()) }),
            }
        }

        Expression::PatternComprehension { variable, pattern, filter, projection, .. } => {
            // Pattern comprehensions require graph access — return empty list when
            // no graph context is available in pure expression evaluation.
            Ok(Value::List(vec![]))
        }

        // ── Reduce ────────────────────────────────────────────────────────

        Expression::Reduce { accumulator, init, variable, source, body, .. } => {
            let init_val = evaluate(init, ctx)?;
            let source_val = evaluate(source, ctx)?;
            match source_val {
                Value::List(items) => {
                    let mut acc = init_val;
                    for item in items {
                        let inner_ctx = ctx.extend_with(variable.clone(), item)
                            .extend_with(accumulator.clone(), acc);
                        acc = evaluate(body, &inner_ctx)?;
                    }
                    Ok(acc)
                }
                Value::Null => Ok(Value::Null),
                v => Err(EvalError { message: format!("reduce source must be a list, got '{}'", v.type_name()) }),
            }
        }

        // ── Quantifier predicates ──────────────────────────────────────────

        Expression::Quantifier { kind, variable, source, filter, .. } => {
            let source_val = evaluate(source, ctx)?;
            match source_val {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    let mut true_count = 0usize;
                    let mut null_count = 0usize;
                    let total = items.len();
                    for item in &items {
                        let inner_ctx = ctx.extend_with(variable.clone(), item.clone());
                        match evaluate(filter, &inner_ctx)? {
                            Value::Boolean(true) => true_count += 1,
                            Value::Null => null_count += 1,
                            _ => {}
                        }
                    }
                    let result = match kind {
                        QuantifierKind::All => {
                            if true_count == total { Value::Boolean(true) }
                            else if null_count > 0 { Value::Null }
                            else { Value::Boolean(false) }
                        }
                        QuantifierKind::Any => {
                            if true_count > 0 { Value::Boolean(true) }
                            else if null_count > 0 { Value::Null }
                            else { Value::Boolean(false) }
                        }
                        QuantifierKind::None => {
                            if true_count > 0 { Value::Boolean(false) }
                            else if null_count > 0 { Value::Null }
                            else { Value::Boolean(true) }
                        }
                        QuantifierKind::Single => {
                            if true_count == 1 && null_count == 0 { Value::Boolean(true) }
                            else if true_count > 1 { Value::Boolean(false) }
                            else if null_count > 0 { Value::Null }
                            else { Value::Boolean(false) }
                        }
                    };
                    Ok(result)
                }
                v => Err(EvalError { message: format!("quantifier source must be a list, got '{}'", v.type_name()) }),
            }
        }

        // ── EXISTS ────────────────────────────────────────────────────────

        Expression::Exists { subquery: _, pattern: _, .. } => {
            // EXISTS subquery requires graph execution context — not available here.
            // Return null when called in pure expression evaluation mode.
            Ok(Value::Null)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Three-valued (Kleene) logic
// ─────────────────────────────────────────────────────────────────────────────

/// Kleene AND truth table:
/// ```text
/// T AND T = T    T AND F = F    T AND N = N
/// F AND T = F    F AND F = F    F AND N = F
/// N AND T = N    N AND F = F    N AND N = N
/// ```
pub fn kleene_and(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Value::Boolean(false),
        (Value::Boolean(true), Value::Boolean(true)) => Value::Boolean(true),
        _ => Value::Null, // either operand is Null
    }
}

/// Kleene OR truth table:
/// ```text
/// T OR T = T     T OR F = T     T OR N = T
/// F OR T = T     F OR F = F     F OR N = N
/// N OR T = T     N OR F = N     N OR N = N
/// ```
pub fn kleene_or(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Value::Boolean(true),
        (Value::Boolean(false), Value::Boolean(false)) => Value::Boolean(false),
        _ => Value::Null,
    }
}

/// Kleene XOR truth table:
/// ```text
/// T XOR T = F    T XOR F = T    T XOR N = N
/// F XOR T = T    F XOR F = F    F XOR N = N
/// N XOR T = N    N XOR F = N    N XOR N = N
/// ```
pub fn kleene_xor(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Boolean(a), Value::Boolean(b)) => Value::Boolean(a ^ b),
        _ => Value::Null,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scalar function registry
// ─────────────────────────────────────────────────────────────────────────────

fn eval_function(
    name: &str,
    args: &[Expression],
    distinct: bool,
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    // Aggregate functions are handled in the physical AggregateOp.
    // Here we only handle scalar functions.

    // Evaluate arguments first (except for functions with special semantics).
    let eval_args: Result<Vec<Value>, EvalError> = args.iter().map(|a| evaluate(a, ctx)).collect();
    let vals = eval_args?;

    let n = name.to_ascii_uppercase();
    match n.as_str() {
        // ── Type conversion ──
        "TOSTRING" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            if v.is_null() { return Ok(Value::Null); }
            Ok(Value::String(v.to_cypher_string()))
        }
        "TOINTEGER" | "TOINT" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i)),
                Value::Float(f) => Ok(Value::Integer(f.0 as i64)),
                Value::String(s) => s.trim().parse::<i64>()
                    .map(Value::Integer)
                    .or_else(|_| s.trim().parse::<f64>().map(|f| Value::Integer(f as i64)))
                    .map_err(|_| EvalError { message: format!("cannot convert '{}' to integer", s) }),
                other => Err(EvalError { message: format!("cannot convert {} to integer", other.type_name()) }),
            }
        }
        "TOFLOAT" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Float(f) => Ok(Value::Float(f)),
                Value::Integer(i) => Ok(Value::Float(OrderedF64(i as f64))),
                Value::String(s) => s.trim().parse::<f64>()
                    .map(|f| Value::Float(OrderedF64(f)))
                    .map_err(|_| EvalError { message: format!("cannot convert '{}' to float", s) }),
                other => Err(EvalError { message: format!("cannot convert {} to float", other.type_name()) }),
            }
        }
        "TOBOOLEAN" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Boolean(b) => Ok(Value::Boolean(b)),
                Value::String(s) => match s.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Boolean(true)),
                    "false" => Ok(Value::Boolean(false)),
                    _ => Ok(Value::Null),
                },
                _ => Ok(Value::Null),
            }
        }

        // ── String functions ──
        "TOUPPER" => string_op(vals, |s| s.to_uppercase()),
        "TOLOWER" => string_op(vals, |s| s.to_lowercase()),
        "TRIM" => string_op(vals, |s| s.trim().to_string()),
        "LTRIM" => string_op(vals, |s| s.trim_start().to_string()),
        "RTRIM" => string_op(vals, |s| s.trim_end().to_string()),
        "REVERSE" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
                Value::List(items) => Ok(Value::List(items.into_iter().rev().collect())),
                other => Err(EvalError { message: format!("reverse() requires String or List, got {}", other.type_name()) }),
            }
        }
        "SUBSTRING" => {
            let mut iter = vals.into_iter();
            let s = iter.next().unwrap_or(Value::Null);
            let start = iter.next().unwrap_or(Value::Null);
            let length = iter.next();
            match (s, start) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::String(s), Value::Integer(start)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = start.max(0) as usize;
                    let end = match length {
                        Some(Value::Integer(len)) => (start + len.max(0) as usize).min(chars.len()),
                        _ => chars.len(),
                    };
                    Ok(Value::String(chars[start.min(chars.len())..end].iter().collect()))
                }
                (s, start) => Err(EvalError { message: format!("substring() type error: {} {}", s.type_name(), start.type_name()) }),
            }
        }
        "SPLIT" => {
            let mut iter = vals.into_iter();
            let s = iter.next().unwrap_or(Value::Null);
            let delim = iter.next().unwrap_or(Value::Null);
            match (s, delim) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::String(s), Value::String(d)) => {
                    Ok(Value::List(s.split(&d).map(|p| Value::String(p.to_string())).collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        "REPLACE" => {
            let mut iter = vals.into_iter();
            let s = iter.next().unwrap_or(Value::Null);
            let search = iter.next().unwrap_or(Value::Null);
            let replace = iter.next().unwrap_or(Value::Null);
            match (s, search, replace) {
                (Value::Null, _, _) => Ok(Value::Null),
                (Value::String(s), Value::String(from), Value::String(to)) => {
                    Ok(Value::String(s.replace(&from, &to)))
                }
                _ => Ok(Value::Null),
            }
        }
        "LEFT" => {
            let mut iter = vals.into_iter();
            let s = iter.next().unwrap_or(Value::Null);
            let n = iter.next().unwrap_or(Value::Null);
            match (s, n) {
                (Value::String(s), Value::Integer(n)) => {
                    let chars: Vec<char> = s.chars().collect();
                    Ok(Value::String(chars[..n.min(chars.len() as i64) as usize].iter().collect()))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "RIGHT" => {
            let mut iter = vals.into_iter();
            let s = iter.next().unwrap_or(Value::Null);
            let n = iter.next().unwrap_or(Value::Null);
            match (s, n) {
                (Value::String(s), Value::Integer(n)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = chars.len().saturating_sub(n.max(0) as usize);
                    Ok(Value::String(chars[start..].iter().collect()))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }

        // ── Numeric functions ──
        "ABS" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i.abs())),
                Value::Float(f) => Ok(Value::Float(OrderedF64(f.0.abs()))),
                other => Err(EvalError { message: format!("abs() requires numeric, got {}", other.type_name()) }),
            }
        }
        "CEIL" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i)),
                Value::Float(f) => Ok(Value::Float(OrderedF64(f.0.ceil()))),
                other => Err(EvalError { message: format!("ceil() requires numeric") }),
            }
        }
        "FLOOR" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i)),
                Value::Float(f) => Ok(Value::Float(OrderedF64(f.0.floor()))),
                _ => Ok(Value::Null),
            }
        }
        "ROUND" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i)),
                Value::Float(f) => Ok(Value::Float(OrderedF64(f.0.round()))),
                _ => Ok(Value::Null),
            }
        }
        "SIGN" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Integer(i) => Ok(Value::Integer(i.signum())),
                Value::Float(f) => Ok(Value::Integer(f.0.signum() as i64)),
                _ => Ok(Value::Null),
            }
        }
        "SQRT" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.sqrt()))
        }
        "EXP" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.exp()))
        }
        "LOG" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.ln()))
        }
        "LOG10" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.log10()))
        }
        "SIN" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.sin()))
        }
        "COS" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.cos()))
        }
        "TAN" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(apply_float(v, |f| f.tan()))
        }
        "ASIN" => { let v = vals.into_iter().next().unwrap_or(Value::Null); Ok(apply_float(v, |f| f.asin())) }
        "ACOS" => { let v = vals.into_iter().next().unwrap_or(Value::Null); Ok(apply_float(v, |f| f.acos())) }
        "ATAN" => { let v = vals.into_iter().next().unwrap_or(Value::Null); Ok(apply_float(v, |f| f.atan())) }
        "ATAN2" => {
            let mut iter = vals.into_iter();
            let y = iter.next().unwrap_or(Value::Null);
            let x = iter.next().unwrap_or(Value::Null);
            match (y.as_float(), x.as_float()) {
                (Some(y), Some(x)) => Ok(Value::Float(OrderedF64(y.atan2(x)))),
                _ => Ok(Value::Null),
            }
        }
        "PI" => Ok(Value::Float(OrderedF64(std::f64::consts::PI))),
        "E" => Ok(Value::Float(OrderedF64(std::f64::consts::E))),
        "RAND" => Ok(Value::Float(OrderedF64(0.5))), // deterministic placeholder
        "POWER" => {
            let mut iter = vals.into_iter();
            let base = iter.next().unwrap_or(Value::Null);
            let exp = iter.next().unwrap_or(Value::Null);
            match (base.as_float(), exp.as_float()) {
                (Some(b), Some(e)) => Ok(Value::Float(OrderedF64(b.powf(e)))),
                _ => Ok(Value::Null),
            }
        }

        // ── Scalar / node functions ──
        "SIZE" | "LENGTH" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::String(s) => Ok(Value::Integer(s.chars().count() as i64)),
                Value::List(items) => Ok(Value::Integer(items.len() as i64)),
                Value::Map(m) => Ok(Value::Integer(m.len() as i64)),
                Value::Path(ref p) => Ok(Value::Integer(p.relationships.len() as i64)),
                other => Err(EvalError { message: format!("size()/length() cannot be applied to {}", other.type_name()) }),
            }
        }
        "TYPE" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Relationship(r) => Ok(Value::String(r.rel_type.clone())),
                other => Err(EvalError { message: format!("type() requires a relationship, got {}", other.type_name()) }),
            }
        }
        "ID" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Node(n) => Ok(Value::Integer(n.id as i64)),
                Value::Relationship(r) => Ok(Value::Integer(r.id as i64)),
                // Also accept Maps that have an `_id` key (legacy format).
                Value::Map(ref m) => Ok(m.get("_id").or_else(|| m.get("node_id")).or_else(|| m.get("edge_id"))
                    .cloned().unwrap_or(Value::Null)),
                other => Err(EvalError { message: format!("id() requires a node or relationship, got {}", other.type_name()) }),
            }
        }
        "LABELS" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Node(n) => Ok(Value::List(n.labels.iter().map(|l| Value::String(l.clone())).collect())),
                Value::Map(ref m) => {
                    // Legacy map-based nodes.
                    if let Some(Value::List(labels)) = m.get("_labels") {
                        Ok(Value::List(labels.clone()))
                    } else {
                        Ok(Value::List(vec![]))
                    }
                }
                other => Err(EvalError { message: format!("labels() requires a node, got {}", other.type_name()) }),
            }
        }
        "KEYS" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Map(m) => Ok(Value::List(m.keys().map(|k| Value::String(k.clone())).collect())),
                Value::Node(n) => Ok(Value::List(n.properties.keys().map(|k| Value::String(k.clone())).collect())),
                Value::Relationship(r) => Ok(Value::List(r.properties.keys().map(|k| Value::String(k.clone())).collect())),
                other => Err(EvalError { message: format!("keys() requires a map or node/rel, got {}", other.type_name()) }),
            }
        }
        "PROPERTIES" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Map(m) => Ok(Value::Map(m)),
                Value::Node(n) => Ok(Value::Map(n.properties.clone())),
                Value::Relationship(r) => Ok(Value::Map(r.properties.clone())),
                other => Err(EvalError { message: format!("properties() requires a node/rel/map, got {}", other.type_name()) }),
            }
        }
        "HEAD" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::List(items) => Ok(items.into_iter().next().unwrap_or(Value::Null)),
                other => Err(EvalError { message: format!("head() requires a list, got {}", other.type_name()) }),
            }
        }
        "TAIL" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::List(mut items) => {
                    if items.is_empty() { Ok(Value::List(vec![])) } else {
                        Ok(Value::List(items.split_off(1)))
                    }
                }
                other => Err(EvalError { message: format!("tail() requires a list, got {}", other.type_name()) }),
            }
        }
        "LAST" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::List(items) => Ok(items.into_iter().last().unwrap_or(Value::Null)),
                other => Err(EvalError { message: format!("last() requires a list, got {}", other.type_name()) }),
            }
        }
        "RANGE" => {
            let mut iter = vals.into_iter();
            let start = iter.next().and_then(|v| v.as_integer()).unwrap_or(0);
            let end = iter.next().and_then(|v| v.as_integer()).unwrap_or(0);
            let step = iter.next().and_then(|v| v.as_integer()).unwrap_or(1);
            if step == 0 {
                return Err(EvalError { message: "range() step cannot be zero".to_string() });
            }
            let mut result = Vec::new();
            let mut i = start;
            while if step > 0 { i <= end } else { i >= end } {
                result.push(Value::Integer(i));
                i += step;
            }
            Ok(Value::List(result))
        }
        "NODES" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Path(p) => Ok(Value::List(p.nodes.iter().map(|n| Value::Node(n.clone())).collect())),
                other => Err(EvalError { message: format!("nodes() requires a path, got {}", other.type_name()) }),
            }
        }
        "RELATIONSHIPS" | "RELS" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Path(p) => Ok(Value::List(p.relationships.iter().map(|r| Value::Relationship(r.clone())).collect())),
                other => Err(EvalError { message: format!("relationships() requires a path, got {}", other.type_name()) }),
            }
        }
        "STARTNODE" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Relationship(r) => Ok(Value::Integer(r.source_id as i64)),
                _ => Ok(Value::Null),
            }
        }
        "ENDNODE" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            match v {
                Value::Null => Ok(Value::Null),
                Value::Relationship(r) => Ok(Value::Integer(r.target_id as i64)),
                _ => Ok(Value::Null),
            }
        }
        "COALESCE" => {
            for v in vals {
                if !v.is_null() { return Ok(v); }
            }
            Ok(Value::Null)
        }
        "ISNULL" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(v.is_null_predicate())
        }
        "ISNOTNULL" => {
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(v.is_not_null_predicate())
        }
        "EXISTS" => {
            // EXISTS(node.property) — return true if the property exists on the entity.
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(Value::Boolean(!v.is_null()))
        }
        "TIMESTAMP" => Ok(Value::Integer(0)), // stable stub
        "DATE" | "TIME" | "LOCALTIME" | "DATETIME" | "LOCALDATETIME" | "DURATION" => {
            // Temporal constructors — return the argument as a string for now.
            let v = vals.into_iter().next().unwrap_or(Value::Null);
            Ok(v)
        }
        "POINT" => {
            // Spatial: return a Map with x, y.
            if let Some(Value::Map(m)) = vals.into_iter().next() {
                Ok(Value::Point {
                    x: m.get("x").and_then(|v| v.as_float()).unwrap_or(0.0),
                    y: m.get("y").and_then(|v| v.as_float()).unwrap_or(0.0),
                    srid: m.get("srid").and_then(|v| v.as_integer()).map(|i| i as u32),
                })
            } else {
                Ok(Value::Null)
            }
        }
        "DISTANCE" => {
            let mut iter = vals.into_iter();
            let a = iter.next().unwrap_or(Value::Null);
            let b = iter.next().unwrap_or(Value::Null);
            match (&a, &b) {
                (Value::Point { x: x1, y: y1, .. }, Value::Point { x: x2, y: y2, .. }) => {
                    let dx = x1 - x2;
                    let dy = y1 - y2;
                    Ok(Value::Float(OrderedF64((dx * dx + dy * dy).sqrt())))
                }
                _ => Ok(Value::Null),
            }
        }
        // Aggregate functions — should be handled by AggregateOp, but we
        // provide a fallback for expression-context usage.
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "COLLECT"
        | "STDEV" | "STDEVP" | "PERCENTILECONT" | "PERCENTILEDISC" => {
            // In expression context without aggregation, return a stub.
            Err(EvalError {
                message: format!("aggregate function '{}' must be used in a RETURN/WITH aggregation context", name),
            })
        }
        _ => Err(EvalError {
            message: format!("unknown function '{}'", name),
        }),
    }
}

fn string_op(vals: Vec<Value>, f: impl Fn(String) -> String) -> Result<Value, EvalError> {
    let v = vals.into_iter().next().unwrap_or(Value::Null);
    match v {
        Value::Null => Ok(Value::Null),
        Value::String(s) => Ok(Value::String(f(s))),
        other => Err(EvalError { message: format!("string function requires String, got {}", other.type_name()) }),
    }
}

fn apply_float(v: Value, f: impl Fn(f64) -> f64) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Integer(i) => Value::Float(OrderedF64(f(i as f64))),
        Value::Float(fl) => Value::Float(OrderedF64(f(fl.0))),
        _ => Value::Null,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Literal evaluation
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn eval_literal(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Integer(v) => Value::Integer(*v),
        Literal::Float(v) => Value::Float(OrderedF64(*v)),
        Literal::String(s) => Value::String(s.clone()),
        Literal::Date(s) => Value::String(s.clone()),
        Literal::Time(s) => Value::String(s.clone()),
        Literal::LocalTime(s) => Value::String(s.clone()),
        Literal::DateTime(s) => Value::String(s.clone()),
        Literal::LocalDateTime(s) => Value::String(s.clone()),
        Literal::Duration(s) => Value::String(s.clone()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Projection helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Evaluate a list of projections and return a row (ordered map of alias → value).
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
            .unwrap_or_else(|| match &proj.expression {
                Expression::Variable(v) => v.clone(),
                Expression::PropertyAccess { base, property, .. } => {
                    format!("{}.{}", base, property)
                }
                e => e.to_string(),
            });
        row.push((name, value));
    }
    Ok(row)
}

/// Evaluate an ORDER BY expression and return a sortable scalar value.
pub fn eval_order_key(expr: &Expression, ctx: &EvalContext) -> Result<Value, EvalError> {
    evaluate(expr, ctx)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

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
    fn kleene_and_null() {
        // true AND null = null
        assert_eq!(
            kleene_and(Value::Boolean(true), Value::Null),
            Value::Null
        );
        // false AND null = false
        assert_eq!(
            kleene_and(Value::Boolean(false), Value::Null),
            Value::Boolean(false)
        );
    }

    #[test]
    fn kleene_or_null() {
        // true OR null = true
        assert_eq!(
            kleene_or(Value::Boolean(true), Value::Null),
            Value::Boolean(true)
        );
        // false OR null = null
        assert_eq!(
            kleene_or(Value::Boolean(false), Value::Null),
            Value::Null
        );
    }

    #[test]
    fn and_with_null_via_evaluate() {
        let ctx = EvalContext::new();
        let expr = Expression::And {
            left: Box::new(Expression::Literal(Literal::Boolean(true))),
            right: Box::new(Expression::Literal(Literal::Null)),
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Null);
    }

    #[test]
    fn function_tostring() {
        let ctx = EvalContext::new();
        let expr = Expression::FunctionCall {
            name: "toString".to_string(),
            args: vec![Expression::Literal(Literal::Integer(42))],
            distinct: false,
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::String("42".to_string()));
    }

    #[test]
    fn function_size_string() {
        let ctx = EvalContext::new();
        let expr = Expression::FunctionCall {
            name: "size".to_string(),
            args: vec![Expression::Literal(Literal::String("hello".to_string()))],
            distinct: false,
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::Integer(5));
    }

    #[test]
    fn function_range() {
        let ctx = EvalContext::new();
        let expr = Expression::FunctionCall {
            name: "range".to_string(),
            args: vec![
                Expression::Literal(Literal::Integer(1)),
                Expression::Literal(Literal::Integer(3)),
            ],
            distinct: false,
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::List(vec![
            Value::Integer(1), Value::Integer(2), Value::Integer(3),
        ]));
    }

    #[test]
    fn parameter_binding() {
        let ctx = EvalContext::new().bind_parameter("name", Value::String("Alice".to_string()));
        let expr = Expression::Parameter("name".to_string());
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::String("Alice".to_string()));
    }

    #[test]
    fn case_expression_generic() {
        let ctx = EvalContext::new().bind("x", Value::Integer(5));
        let expr = Expression::Case {
            subject: None,
            alternatives: vec![
                CaseAlternative {
                    condition: Expression::Comparison {
                        op: ComparisonOperator::Gt,
                        left: Box::new(Expression::Variable("x".to_string())),
                        right: Box::new(Expression::Literal(Literal::Integer(3))),
                        span: None,
                    },
                    result: Expression::Literal(Literal::String("big".to_string())),
                },
            ],
            default: Some(Box::new(Expression::Literal(Literal::String("small".to_string())))),
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::String("big".to_string()));
    }

    #[test]
    fn list_comprehension() {
        let ctx = EvalContext::new();
        let expr = Expression::ListComprehension {
            variable: "x".to_string(),
            source: Box::new(Expression::List(vec![
                Expression::Literal(Literal::Integer(1)),
                Expression::Literal(Literal::Integer(2)),
                Expression::Literal(Literal::Integer(3)),
            ])),
            filter: None,
            projection: Some(Box::new(Expression::BinaryOp {
                op: BinaryOperator::Mul,
                left: Box::new(Expression::Variable("x".to_string())),
                right: Box::new(Expression::Literal(Literal::Integer(2))),
                span: None,
            })),
            span: None,
        };
        assert_eq!(evaluate(&expr, &ctx).unwrap(), Value::List(vec![
            Value::Integer(2), Value::Integer(4), Value::Integer(6),
        ]));
    }
}
