//! Cypher value types and runtime semantics.
//!
//! This module defines the runtime value representation used by the query
//! execution engine.  It faithfully models openCypher null semantics,
//! type coercion rules, and container types — including the entity types
//! `NODE`, `RELATIONSHIP`, and `PATH`, and the spatial type `POINT`.

use crate::graph::property::{OrderedF64, Property};
use std::collections::HashMap;
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// Entity value types (in-memory representation of matched entities)
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory view of a matched node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeValue {
    /// Unique logical node id.
    pub id: u64,
    /// Label strings (resolved from catalog).
    pub labels: Vec<String>,
    /// Property key → runtime value map.
    pub properties: HashMap<String, Value>,
}

/// In-memory view of a matched relationship.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipValue {
    /// Unique logical edge id.
    pub id: u64,
    /// Relationship type string.
    pub rel_type: String,
    /// Logical id of the source node.
    pub source_id: u64,
    /// Logical id of the target node.
    pub target_id: u64,
    /// Property key → runtime value map.
    pub properties: HashMap<String, Value>,
}

/// An ordered sequence of alternating nodes and relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct PathValue {
    /// Nodes in order (length = hops + 1).
    pub nodes: Vec<NodeValue>,
    /// Relationships in order (length = hops).
    pub relationships: Vec<RelationshipValue>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Main Value enum
// ─────────────────────────────────────────────────────────────────────────────

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
    // Entity types.
    Node(NodeValue),
    Relationship(RelationshipValue),
    Path(PathValue),
    // Spatial type.
    Point { x: f64, y: f64, srid: Option<u32> },
}

impl Value {
    // ── Type predicates ───────────────────────────────────────────────────

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
            _ => Value::Null,
        }
    }

    /// Attempt to coerce this value to `i64`.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(v) => Some(*v),
            Value::Float(f) => Some(f.0 as i64),
            _ => None,
        }
    }

    /// Attempt to coerce this value to `f64`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Integer(v) => Some(*v as f64),
            Value::Float(f) => Some(f.0),
            _ => None,
        }
    }

    /// Attempt to coerce this value to a `String`.
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Return the Cypher type name for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Boolean(_) => "Boolean",
            Value::Integer(_) => "Integer",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Node(_) => "Node",
            Value::Relationship(_) => "Relationship",
            Value::Path(_) => "Path",
            Value::Point { .. } => "Point",
        }
    }

    /// Convert to a Cypher-style string representation.
    pub fn to_cypher_string(&self) -> std::string::String {
        match self {
            Value::Null => "null".to_string(),
            Value::Boolean(b) => b.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Float(f) => f.0.to_string(),
            Value::String(s) => s.clone(),
            Value::List(items) => {
                let elems: Vec<_> = items.iter().map(|v| v.to_cypher_string()).collect();
                format!("[{}]", elems.join(", "))
            }
            Value::Map(m) => {
                let elems: Vec<_> = m.iter().map(|(k, v)| format!("{}: {}", k, v.to_cypher_string())).collect();
                format!("{{{}}}", elems.join(", "))
            }
            Value::Node(n) => format!("({{id: {}}})", n.id),
            Value::Relationship(r) => format!("[{{id: {}}}]", r.id),
            Value::Path(_) => "<path>".to_string(),
            Value::Point { x, y, .. } => format!("point({{x: {}, y: {}}})", x, y),
        }
    }

    // ── Arithmetic ────────────────────────────────────────────────────────

    /// Numeric addition with Cypher null semantics.
    /// Also supports string concatenation.
    pub fn add(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() {
            return Some(Value::Null);
        }
        match (self, rhs) {
            // Use checked arithmetic so an overflowing request (e.g.
            // `RETURN <i64::MAX> + 1`) yields a typed evaluation error upstream
            // instead of panicking in debug or silently wrapping in release.
            (Value::Integer(a), Value::Integer(b)) => a.checked_add(*b).map(Value::Integer),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 + b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 + *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 + b.0))),
            (Value::String(a), Value::String(b)) => Some(Value::String(format!("{}{}", a, b))),
            (Value::List(a), Value::List(b)) => {
                let mut v = a.clone();
                v.extend_from_slice(b);
                Some(Value::List(v))
            }
            _ => None,
        }
    }

    pub fn sub(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => a.checked_sub(*b).map(Value::Integer),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 - b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 - *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 - b.0))),
            _ => None,
        }
    }

    pub fn mul(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => a.checked_mul(*b).map(Value::Integer),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 * b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 * *b as f64))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 * b.0))),
            _ => None,
        }
    }

    /// Numeric division. Division by zero returns `Null`.
    pub fn div(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(_), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            // `checked_div` also guards the `i64::MIN / -1` overflow case; on
            // overflow we fall back to Null rather than panicking.
            (Value::Integer(a), Value::Integer(b)) => {
                Some(a.checked_div(*b).map_or(Value::Null, Value::Integer))
            }
            (Value::Integer(a), Value::Float(b)) if b.0 == 0.0 => Some(Value::Null),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 / b.0))),
            (Value::Float(a), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 / *b as f64))),
            (Value::Float(a), Value::Float(b)) if b.0 == 0.0 => Some(Value::Null),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 / b.0))),
            _ => None,
        }
    }

    pub fn modulo(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(_), Value::Integer(b)) if *b == 0 => Some(Value::Null),
            // `checked_rem` guards the `i64::MIN % -1` overflow case.
            (Value::Integer(a), Value::Integer(b)) => {
                Some(a.checked_rem(*b).map_or(Value::Null, Value::Integer))
            }
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0 % b.0))),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64(*a as f64 % b.0))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0 % *b as f64))),
            _ => None,
        }
    }

    pub fn pow(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Float(OrderedF64((*a as f64).powf(*b as f64)))),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Float(OrderedF64((*a as f64).powf(b.0)))),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Float(OrderedF64(a.0.powf(*b as f64)))),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(OrderedF64(a.0.powf(b.0)))),
            _ => None,
        }
    }

    /// Logical NOT with Kleene three-valued semantics.
    pub fn kleene_not(&self) -> Option<Value> {
        match self {
            Value::Null => Some(Value::Null),
            Value::Boolean(b) => Some(Value::Boolean(!b)),
            _ => None,
        }
    }

    /// Legacy: logical NOT (same as kleene_not but kept for compat).
    pub fn not(&self) -> Option<Value> {
        self.kleene_not()
    }

    pub fn negate(&self) -> Option<Value> {
        match self {
            Value::Null => Some(Value::Null),
            // `checked_neg` guards against negating `i64::MIN`, which would
            // overflow and panic.
            Value::Integer(v) => v.checked_neg().map(Value::Integer),
            Value::Float(f) => Some(Value::Float(OrderedF64(-f.0))),
            _ => None,
        }
    }

    // ── Comparisons ────────────────────────────────────────────────────────

    /// openCypher equality: numeric cross-type, null propagation.
    /// `1 = 1.0` is `true`.
    pub fn cypher_eq(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        let result = match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.0 == b.0,
            // Cross-type numeric equality: 1 = 1.0 → true.
            (Value::Integer(a), Value::Float(b)) => (*a as f64) == b.0,
            (Value::Float(a), Value::Integer(b)) => a.0 == (*b as f64),
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                if a.len() != b.len() { return Some(Value::Boolean(false)); }
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.cypher_eq(y) {
                        Some(Value::Boolean(true)) => {}
                        _ => return Some(Value::Boolean(false)),
                    }
                }
                true
            }
            _ => return Some(Value::Boolean(false)),
        };
        Some(Value::Boolean(result))
    }

    pub fn cypher_ne(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match self.cypher_eq(rhs) {
            Some(Value::Boolean(b)) => Some(Value::Boolean(!b)),
            other => other,
        }
    }

    /// Legacy equality (delegates to cypher_eq).
    pub fn eq(&self, rhs: &Value) -> Option<Value> {
        self.cypher_eq(rhs)
    }

    pub fn ne(&self, rhs: &Value) -> Option<Value> {
        self.cypher_ne(rhs)
    }

    pub fn lt(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a < b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) < b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 < *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 < b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a < b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(a < b)),
            _ => None,
        }
    }

    pub fn le(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a <= b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) <= b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 <= *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 <= b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a <= b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(a <= b)),
            _ => None,
        }
    }

    pub fn gt(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a > b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) > b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 > *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 > b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a > b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(a > b)),
            _ => None,
        }
    }

    pub fn ge(&self, rhs: &Value) -> Option<Value> {
        if self.is_null() || rhs.is_null() { return Some(Value::Null); }
        match (self, rhs) {
            (Value::Integer(a), Value::Integer(b)) => Some(Value::Boolean(a >= b)),
            (Value::Integer(a), Value::Float(b)) => Some(Value::Boolean((*a as f64) >= b.0)),
            (Value::Float(a), Value::Integer(b)) => Some(Value::Boolean(a.0 >= *b as f64)),
            (Value::Float(a), Value::Float(b)) => Some(Value::Boolean(a.0 >= b.0)),
            (Value::String(a), Value::String(b)) => Some(Value::Boolean(a >= b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(a >= b)),
            _ => None,
        }
    }

    pub fn is_null_predicate(&self) -> Value {
        Value::Boolean(self.is_null())
    }

    pub fn is_not_null_predicate(&self) -> Value {
        Value::Boolean(!self.is_null())
    }

    // ── Cypher orderability ────────────────────────────────────────────────

    /// Compare two values for ordering purposes following the openCypher spec:
    /// Numbers (mixed), Strings, Booleans, Points, then Null (always last).
    ///
    /// Returns `None` when the types are incomparable.
    pub fn cypher_compare(&self, rhs: &Value) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match (self, rhs) {
            // Null is always last.
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Null, _) => Some(Ordering::Greater),
            (_, Value::Null) => Some(Ordering::Less),
            // Numeric (cross-type).
            (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => Some(a.partial_cmp(b).unwrap_or(Ordering::Equal)),
            (Value::Integer(a), Value::Float(b)) => Some((*a as f64).partial_cmp(&b.0).unwrap_or(Ordering::Equal)),
            (Value::Float(a), Value::Integer(b)) => Some(a.0.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal)),
            // String.
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            // Boolean (false < true).
            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            // List: element-wise.
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.cypher_compare(y) {
                        Some(Ordering::Equal) => continue,
                        other => return other,
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Display
// ─────────────────────────────────────────────────────────────────────────────

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
                let mut sorted: Vec<_> = entries.iter().collect();
                sorted.sort_by_key(|(k, _)| k.as_str());
                let elems: Vec<String> = sorted.iter().map(|(k, v)| format!("{}: {}", k, v)).collect();
                write!(f, "{{{}}}", elems.join(", "))
            }
            Value::Node(n) => {
                write!(f, "({}", n.id)?;
                if !n.labels.is_empty() { write!(f, ":{}",  n.labels.join(":"))?; }
                write!(f, ")")
            }
            Value::Relationship(r) => write!(f, "[:{} id={}]", r.rel_type, r.id),
            Value::Path(_) => write!(f, "<path>"),
            Value::Point { x, y, .. } => write!(f, "point({{x: {}, y: {}}})", x, y),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// From Property
// ─────────────────────────────────────────────────────────────────────────────

impl From<Property> for Value {
    fn from(prop: Property) -> Self {
        Value::from_property(prop)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_propagates_through_arithmetic() {
        let a = Value::Integer(5);
        let null = Value::Null;
        assert_eq!(a.add(&null).unwrap(), Value::Null);
        assert_eq!(null.sub(&a).unwrap(), Value::Null);
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
    fn integer_overflow_is_none_not_panic() {
        // Checked arithmetic returns None (→ typed eval error upstream) rather
        // than panicking on overflow.
        assert_eq!(Value::Integer(i64::MAX).add(&Value::Integer(1)), None);
        assert_eq!(Value::Integer(i64::MIN).sub(&Value::Integer(1)), None);
        assert_eq!(Value::Integer(i64::MAX).mul(&Value::Integer(2)), None);
        assert_eq!(Value::Integer(i64::MIN).negate(), None);
    }

    #[test]
    fn min_div_neg_one_returns_null_not_panic() {
        // i64::MIN / -1 overflows; we yield Null instead of panicking.
        assert_eq!(
            Value::Integer(i64::MIN).div(&Value::Integer(-1)).unwrap(),
            Value::Null
        );
        assert_eq!(
            Value::Integer(i64::MIN).modulo(&Value::Integer(-1)).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn comparison_with_null_returns_null() {
        let a = Value::Integer(5);
        let null = Value::Null;
        assert_eq!(a.cypher_eq(&null).unwrap(), Value::Null);
        assert_eq!(a.lt(&null).unwrap(), Value::Null);
    }

    #[test]
    fn equality_between_integers() {
        let a = Value::Integer(42);
        let b = Value::Integer(42);
        let c = Value::Integer(7);
        assert_eq!(a.cypher_eq(&b).unwrap(), Value::Boolean(true));
        assert_eq!(a.cypher_eq(&c).unwrap(), Value::Boolean(false));
    }

    #[test]
    fn cross_type_numeric_equality() {
        // openCypher: 1 = 1.0 is true.
        let a = Value::Integer(1);
        let b = Value::Float(OrderedF64(1.0));
        assert_eq!(a.cypher_eq(&b).unwrap(), Value::Boolean(true));
    }

    #[test]
    fn logical_not() {
        assert_eq!(Value::Boolean(true).kleene_not().unwrap(), Value::Boolean(false));
        assert_eq!(Value::Boolean(false).kleene_not().unwrap(), Value::Boolean(true));
        assert_eq!(Value::Null.kleene_not().unwrap(), Value::Null);
    }

    #[test]
    fn numeric_negation() {
        assert_eq!(Value::Integer(5).negate().unwrap(), Value::Integer(-5));
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

    #[test]
    fn node_value_display() {
        let n = NodeValue {
            id: 1,
            labels: vec!["Person".to_string()],
            properties: HashMap::new(),
        };
        let v = Value::Node(n);
        assert!(v.to_string().contains("1"));
    }

    #[test]
    fn cypher_orderability_null_last() {
        use std::cmp::Ordering;
        let a = Value::Integer(5);
        let null = Value::Null;
        assert_eq!(a.cypher_compare(&null), Some(Ordering::Less));
        assert_eq!(null.cypher_compare(&a), Some(Ordering::Greater));
    }
}
