//! Cypher value types and runtime semantics.
//!
//! This module defines the runtime value representation used by the query
//! execution engine.  It faithfully models openCypher null semantics,
//! type coercion rules, and container types.

use crate::graph::property::{OrderedF64, Property};
use std::collections::HashMap;
use std::fmt;

/// A runtime value in the Cypher query engine.
///
/// openCypher is dynamically typed, so a single value can be any of the
/// supported types.  `Null` is a first-class value and propagates through
/// almost every operation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(OrderedF64),
    String(String),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
    // TODO: Date, Time, LocalTime, DateTime, LocalDateTime, Duration, Point
}

impl Value {
    /// Returns `true` if this value is `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Convert a domain [`Property`] into a runtime [`Value`].
    pub fn from_property(prop: Property) -> Self {
        match prop {
            Property::Null => Value::Null,
            Property::Boolean(b) => Value::Boolean(b),
            Property::Integer(v) => Value::Integer(v),
            Property::Float(f) => Value::Float(f),
            Property::String(s) => Value::String(s),
            // Lists and maps from Property are not yet supported.
            _ => Value::Null,
        }
    }

    /// Attempt to coerce this value to `i64`.
    ///
    /// Floats are truncated toward zero.  `Null` returns `None`.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(v) => Some(*v),
            Value::Float(f) => Some(f.0 as i64),
            _ => None,
        }
    }

    /// Attempt to coerce this value to `f64`.
    ///
    /// Integers are promoted.  `Null` returns `None`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Integer(v) => Some(*v as f64),
            Value::Float(f) => Some(f.0),
            _ => None,
        }
    }

    /// Attempt to coerce this value to `String`.
    ///
    /// openCypher does **not** auto-coerce to string in expressions,
    /// so this is used only for explicit `toString()` or display.
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Return the Cypher type name for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Boolean(_) => "Boolean",
            Value::Integer(_) => "Integer",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
        }
    }

    /// Numeric addition with Cypher null semantics.
    ///
    /// Rules:
    /// * `Null + x` → `Null`
    /// * Integer + Integer → Integer
    /// * Integer + Float   → Float
    /// * Float   + Float   → Float
    /// * otherwise → `None` (type error)
    pub fn add(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(a + b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 + b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 + *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 + b.0))),
            (Value::String(a), Value::String(b)) => Some(Value::String(format!("{}{}", a, b))),
            _ => None,
        }
    }

    /// Numeric subtraction with Cypher null semantics.
    pub fn sub(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(a - b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 - b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 - *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 - b.0))),
            _ => None,
        }
    }

    /// Numeric multiplication with Cypher null semantics.
    pub fn mul(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(a * b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 * b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 * *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 * b.0))),
            _ => None,
        }
    }

    /// Numeric division with Cypher null semantics.
    ///
    /// Division by zero returns `Null` (not an error) per openCypher.
    pub fn div(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(_), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(a / b)),
            (Value::Integer(a), Value::Float(b)) if b.0 == 0.0 => Some(Value::Null),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 / b.0))),
            (Value::Float(a), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 / *b as f64))),
            (Value::Float(a), Value::Float(b)) if b.0 == 0.0 => Some(Value::Null),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 / b.0))),
            _ => None,
        }
    }

    /// Numeric modulo with Cypher null semantics.
    pub fn modulo(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(_), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(a % b)),
            _ => None,
        }
    }

    /// Numeric exponentiation with Cypher null semantics.
    pub fn pow(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Float(OrderedF64((*a as f64).powf(*b as f64)))),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64((*a as f64).powf(b.0)))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0.powf(*b as f64)))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0.powf(b.0)))),
            _ => None,
        }
    }

    /// Logical negation (`NOT`) with Cypher null semantics.
    pub fn not(&self) -> Option<Value> {
        match self {
            Value::Null => Some(Value::Null),
            Value::Boolean(b) => Some(Value::Boolean(!b)),
            _ => None,
        }
    }

    /// Numeric negation (`-expr`) with Cypher null semantics.
    pub fn negate(&self) -> Option<Value> {
        match self {
            Value::Null => Some(Value::Null),
            Value::Integer(v) => Some(Value::Integer(-v)),
            Value::Float(f) => Some(Value::Float(OrderedF64(-f.0))),
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // Comparisons
    // ------------------------------------------------------------------

    /// Equality comparison with Cypher null semantics.
    ///
    /// `Null = x` and `x = Null` both return `Null` (not `true` or `false`).
    pub fn eq(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        Some(Value::Boolean(self == rhs))
    }

    /// Inequality comparison with Cypher null semantics.
    pub fn ne(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        Some(Value::Boolean(self != rhs))
    }

    /// Less-than comparison with Cypher null semantics.
    pub fn lt(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a < b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) < b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 < *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 < b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a < b)),
            _ => None,
        }
    }

    /// Less-than-or-equal comparison with Cypher null semantics.
    pub fn le(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a <= b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) <= b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 <= *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 <= b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a <= b)),
            _ => None,
        }
    }

    /// Greater-than comparison with Cypher null semantics.
    pub fn gt(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a > b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) > b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 > *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 > b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a > b)),
            _ => None,
        }
    }

    /// Greater-than-or-equal comparison with Cypher null semantics.
    pub fn ge(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a >= b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) >= b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 >= *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 >= b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a >= b)),
            _ => None,
        }
    }

    /// `IS NULL` predicate.
    pub fn is_null_predicate(&self) -> Value {
        Value::Boolean(self.is_null())
    }

    /// `IS NOT NULL` predicate.
    pub fn is_not_null_predicate(&self) -> Value {
        Value::Boolean(!self.is_null())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Integer(v) => write!(f, "{}", v),
            Value::Float(fl) => write!(f, "{}", fl.0),
            Value::String(s) => write!(f, "'{}'", s),
            Value::List(items) => {
                let elems: Vec<String> = items.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", elems.join(", "))
            }
            Value::Map(entries) => {
                let elems: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect();
                write!(f, "{{{}}}", elems.join(", "))
            }
        }
    }
}

impl From<Property> for Value {
    fn from(prop: Property) -> Self {
        Value::from_property(prop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_propagates_through_arithmetic() {
        let a = Value::Integer(5);
        let null = Value::Null;
        assert_eq!(a.add(&null).unwrap(), Value::Null);
        assert_eq!(null.sub(&a).unwrap(), Value::Null);
        assert_eq!(null.mul(&null).unwrap(), Value::Null);
    }

    #[test]
    fn integer_addition() {
        let a = Value::Integer(3);
        let b = Value::Integer(7);
        assert_eq!(a.add(&b).unwrap(), Value::Integer(10));
    }

    #[test]
    fn mixed_type_addition_promotes_to_float() {
        let a = Value::Integer(3);
        let b = Value::Float(OrderedF64(2.5));
        assert_eq!(a.add(&b).unwrap(), Value::Float(OrderedF64(5.5)));
    }

    #[test]
    fn string_concatenation() {
        let a = Value::String("hello".to_string());
        let b = Value::String("world".to_string());
        assert_eq!(a.add(&b).unwrap(), Value::String("helloworld".to_string()));
    }

    #[test]
    fn division_by_zero_returns_null() {
        let a = Value::Integer(10);
        let b = Value::Integer(0);
        assert_eq!(a.div(&b).unwrap(), Value::Null);
    }

    #[test]
    fn comparison_with_null_returns_null() {
        let a = Value::Integer(5);
        let null = Value::Null;
        assert_eq!(a.eq(&null).unwrap(), Value::Null);
        assert_eq!(a.lt(&null).unwrap(), Value::Null);
    }

    #[test]
    fn equality_between_integers() {
        let a = Value::Integer(42);
        let b = Value::Integer(42);
        let c = Value::Integer(7);
        assert_eq!(a.eq(&b).unwrap(), Value::Boolean(true));
        assert_eq!(a.eq(&c).unwrap(), Value::Boolean(false));
    }

    #[test]
    fn logical_not() {
        assert_eq!(Value::Boolean(true).not().unwrap(), Value::Boolean(false));
        assert_eq!(Value::Boolean(false).not().unwrap(), Value::Boolean(true));
        assert_eq!(Value::Null.not().unwrap(), Value::Null);
    }

    #[test]
    fn numeric_negation() {
        assert_eq!(Value::Integer(5).negate().unwrap(), Value::Integer(-5));
        assert_eq!(Value::Float(OrderedF64(3.14)).negate().unwrap(), Value::Float(OrderedF64(-3.14)));
        assert_eq!(Value::Null.negate().unwrap(), Value::Null);
    }

    #[test]
    fn is_null_predicate() {
        assert_eq!(Value::Null.is_null_predicate(), Value::Boolean(true));
        assert_eq!(Value::Integer(1).is_null_predicate(), Value::Boolean(false));
    }

    #[test]
    fn list_display() {
        let v = Value::List(vec![Value::Integer(1), Value::Null, Value::String("a".to_string())]);
        assert_eq!(v.to_string(), "[1, NULL, 'a']");
    }

    #[test]
    fn map_display() {
        let mut m = HashMap::new();
        m.insert("k".to_string(), Value::Integer(42));
        let v = Value::Map(m);
        assert_eq!(v.to_string(), "{k: 42}");
    }
}
