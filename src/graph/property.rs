//! Cypher property value enum with total equality and `From` conversions.
//!
//! The [`Property`] enum represents every scalar and composite type that can
//! appear in an openCypher property map.  It is the primary *domain* type
//! used by the query engine, distinct from the low-level storage format
//! ([`PropertyRecord`]) and the ordered binary codec ([`Value`]).
//!
//! # Design notes
//!
//! * `Float` stores an `f64` directly; equality uses `==` so that `NaN != NaN`,
//!   matching Cypher semantics.
//! * `Null` is a singleton variant.
//! * `List` and `Map` are heap-allocated so that the enum itself remains small
//!   for the common scalar cases.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A Cypher property value.
///
/// Covers all scalar and composite types required by the openCypher TCK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
    /// The Cypher `NULL` value.
    Null,
    /// A 64-bit signed integer.
    Integer(i64),
    /// An IEEE-754 double.  Equality follows Cypher: `NaN != NaN`.
    Float(OrderedF64),
    /// A Unicode string.
    String(String),
    /// A boolean.
    Boolean(bool),
    /// An instant in time (epoch milliseconds).
    Date(i64),
    /// A temporal duration (nanoseconds).
    Duration(i64),
    /// A 2-D or 3-D geospatial point (SRID, x, y, optional z).
    Point { srid: u32, x: OrderedF64, y: OrderedF64, z: Option<OrderedF64> },
    /// A homogeneous or heterogeneous list.
    List(Vec<Property>),
    /// A string-keyed map.
    Map(HashMap<String, Property>),
}

// HashMap does not implement Hash, so we implement Hash manually for Property.
impl Hash for Property {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Property::Null => {}
            Property::Integer(v) => v.hash(state),
            Property::Float(v) => v.hash(state),
            Property::String(v) => v.hash(state),
            Property::Boolean(v) => v.hash(state),
            Property::Date(v) => v.hash(state),
            Property::Duration(v) => v.hash(state),
            Property::Point { srid, x, y, z } => {
                srid.hash(state);
                x.hash(state);
                y.hash(state);
                z.hash(state);
            }
            Property::List(v) => {
                for item in v {
                    item.hash(state);
                }
            }
            Property::Map(v) => {
                let mut entries: Vec<(&String, &Property)> = v.iter().collect();
                entries.sort_by_key(|(k, _)| *k);
                for (k, item) in entries {
                    k.hash(state);
                    item.hash(state);
                }
            }
        }
    }
}

impl Property {
    /// Returns `true` if this is the `Null` variant.
    pub fn is_null(&self) -> bool {
        matches!(self, Property::Null)
    }

    /// Returns `true` if this is a scalar (not `List`, `Map`, or `Null`).
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Property::Integer(_)
                | Property::Float(_)
                | Property::String(_)
                | Property::Boolean(_)
                | Property::Date(_)
                | Property::Duration(_)
                | Property::Point { .. }
        )
    }

    /// Returns `true` if this is a composite (`List` or `Map`).
    pub fn is_composite(&self) -> bool {
        matches!(self, Property::List(_) | Property::Map(_))
    }

    /// Total ordering compatible with Cypher `ORDER BY`.
    ///
    /// Order: `Null < Boolean < Integer < Float < String < List < Map < Point < Date < Duration`
    pub fn discriminant_ord(&self) -> u8 {
        match self {
            Property::Null => 0,
            Property::Boolean(_) => 1,
            Property::Integer(_) => 2,
            Property::Float(_) => 3,
            Property::String(_) => 4,
            Property::List(_) => 5,
            Property::Map(_) => 6,
            Property::Point { .. } => 7,
            Property::Date(_) => 8,
            Property::Duration(_) => 9,
        }
    }
}

// ------------------------------------------------------------------
// From conversions
// ------------------------------------------------------------------

impl From<i64> for Property {
    fn from(v: i64) -> Self {
        Property::Integer(v)
    }
}

impl From<i32> for Property {
    fn from(v: i32) -> Self {
        Property::Integer(v as i64)
    }
}

impl From<u32> for Property {
    fn from(v: u32) -> Self {
        Property::Integer(v as i64)
    }
}

impl From<f64> for Property {
    fn from(v: f64) -> Self {
        Property::Float(OrderedF64(v))
    }
}

impl From<f32> for Property {
    fn from(v: f32) -> Self {
        Property::Float(OrderedF64(v as f64))
    }
}

impl From<String> for Property {
    fn from(v: String) -> Self {
        Property::String(v)
    }
}

impl From<&str> for Property {
    fn from(v: &str) -> Self {
        Property::String(v.to_owned())
    }
}

impl From<bool> for Property {
    fn from(v: bool) -> Self {
        Property::Boolean(v)
    }
}

impl From<Vec<Property>> for Property {
    fn from(v: Vec<Property>) -> Self {
        Property::List(v)
    }
}

impl From<HashMap<String, Property>> for Property {
    fn from(v: HashMap<String, Property>) -> Self {
        Property::Map(v)
    }
}

// ------------------------------------------------------------------
// OrderedF64: wraps f64 so it can be Eq + Hash + Ord.
//
// NaN is canonicalised to a single quiet-NaN bit pattern, so that all NaNs
// compare equal and hash identically.  This is needed for HashMap keys but
// *not* for Cypher value equality (where NaN != NaN).
// ------------------------------------------------------------------

/// Wrapper around `f64` that provides total ordering and hashing.
///
/// **Caution:** This canonicalises NaN to a single bit pattern.  For Cypher
/// semantics (`NaN != NaN`) use the raw `f64` directly.
#[derive(Debug, Clone, Copy)]
pub struct OrderedF64(pub f64);

impl OrderedF64 {
    /// Canonical NaN bit pattern used for hashing and ordering.
    pub const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

    fn canonicalise_bits(bits: u64) -> u64 {
        // NaN: exponent bits are all 1s (0x7FF) AND mantissa is non-zero.
        let exponent_mask = 0x7FF0_0000_0000_0000u64;
        let mantissa_mask = 0x000F_FFFF_FFFF_FFFFu64;
        if (bits & exponent_mask) == exponent_mask && (bits & mantissa_mask) != 0 {
            // Any NaN -> canonical quiet NaN.
            Self::CANONICAL_NAN_BITS
        } else {
            bits
        }
    }

    /// Total ordering: negative < -0 < +0 < positive < NaN.
    pub fn total_cmp(&self, other: &Self) -> std::cmp::Ordering {
        let a = Self::canonicalise_bits(self.0.to_bits());
        let b = Self::canonicalise_bits(other.0.to_bits());
        // IEEE-754 total order via bit-pattern manipulation.
        let a_neg = a & 0x8000_0000_0000_0000 != 0;
        let b_neg = b & 0x8000_0000_0000_0000 != 0;
        if a == b {
            return std::cmp::Ordering::Equal;
        }
        if a == Self::CANONICAL_NAN_BITS {
            return std::cmp::Ordering::Greater;
        }
        if b == Self::CANONICAL_NAN_BITS {
            return std::cmp::Ordering::Less;
        }
        match (a_neg, b_neg) {
            (true, true) => b.cmp(&a), // both negative: reverse order
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => a.cmp(&b),
        }
    }
}

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        // In Cypher, NaN != NaN.  We replicate that by delegating to raw f64.
        self.0 == other.0
    }
}

impl Eq for OrderedF64 {}

impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.total_cmp(other))
    }
}

impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.total_cmp(other)
    }
}

impl Hash for OrderedF64 {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Self::canonicalise_bits(self.0.to_bits()).hash(state);
    }
}

impl fmt::Display for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Property::Null => write!(f, "NULL"),
            Property::Integer(v) => write!(f, "{}", v),
            Property::Float(v) => write!(f, "{}", v.0),
            Property::String(v) => write!(f, "'{}'", v),
            Property::Boolean(v) => write!(f, "{}", v),
            Property::Date(v) => write!(f, "date({})", v),
            Property::Duration(v) => write!(f, "duration({})", v),
            Property::Point { srid, x, y, z } => {
                if let Some(z_val) = z {
                    write!(f, "point({{srid:{}, x:{}, y:{}, z:{}}})", srid, x.0, y.0, z_val.0)
                } else {
                    write!(f, "point({{srid:{}, x:{}, y:{}}})", srid, x.0, y.0)
                }
            }
            Property::List(v) => {
                write!(f, "[")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Property::Map(v) => {
                write!(f, "{{")?;
                let mut entries: Vec<(&String, &Property)> = v.iter().collect();
                entries.sort_by_key(|(k, _)| *k);
                for (i, (k, item)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, item)?;
                }
                write!(f, "}}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_i64() {
        let p: Property = 42i64.into();
        assert_eq!(p, Property::Integer(42));
    }

    #[test]
    fn from_f64() {
        let p: Property = 3.14.into();
        assert_eq!(p, Property::Float(OrderedF64(3.14)));
    }

    #[test]
    fn from_string() {
        let p: Property = "hello".into();
        assert_eq!(p, Property::String("hello".to_string()));
    }

    #[test]
    fn from_bool() {
        let p: Property = true.into();
        assert_eq!(p, Property::Boolean(true));
    }

    #[test]
    fn from_list() {
        let p: Property = vec![Property::Integer(1), Property::Integer(2)].into();
        assert_eq!(
            p,
            Property::List(vec![Property::Integer(1), Property::Integer(2)])
        );
    }

    #[test]
    fn from_map() {
        let mut m = HashMap::new();
        m.insert("name".to_string(), Property::String("Alice".to_string()));
        let p: Property = m.clone().into();
        assert_eq!(p, Property::Map(m));
    }

    #[test]
    fn null_singleton() {
        let a = Property::Null;
        let b = Property::Null;
        assert_eq!(a, b);
        assert!(a.is_null());
    }

    #[test]
    fn is_scalar_and_composite() {
        assert!(Property::Integer(1).is_scalar());
        assert!(Property::Boolean(false).is_scalar());
        assert!(!Property::Null.is_scalar());
        assert!(!Property::List(vec![]).is_scalar());
        assert!(Property::List(vec![]).is_composite());
        assert!(Property::Map(HashMap::new()).is_composite());
    }

    #[test]
    fn nan_inequality_cypher_semantics() {
        let a = Property::Float(OrderedF64(f64::NAN));
        let b = Property::Float(OrderedF64(f64::NAN));
        // Cypher: NaN != NaN
        assert_ne!(a, b, "NaN must not equal NaN in Cypher semantics");
    }

    #[test]
    fn ordered_f64_total_order() {
        let values = [
            OrderedF64(f64::NEG_INFINITY),
            OrderedF64(-1000.0),
            OrderedF64(-1.0),
            OrderedF64(-0.0),
            OrderedF64(0.0),
            OrderedF64(1.0),
            OrderedF64(1000.0),
            OrderedF64(f64::INFINITY),
            OrderedF64(f64::NAN),
        ];
        for i in 1..values.len() {
            assert!(
                values[i - 1] < values[i],
                "total order violated at index {}: {:?} vs {:?}",
                i,
                values[i - 1].0,
                values[i].0
            );
        }
    }

    #[test]
    fn ordered_f64_hash_canonicalises_nan() {
        let a = OrderedF64(f64::NAN);
        let b = OrderedF64(f64::from_bits(0x7FF4_0000_0000_0000)); // signaling NaN
        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        b.hash(&mut hasher_b);
        assert_eq!(hasher_a.finish(), hasher_b.finish(), "all NaNs should hash the same");
    }

    #[test]
    fn display_null() {
        assert_eq!(format!("{}", Property::Null), "NULL");
    }

    #[test]
    fn display_integer() {
        assert_eq!(format!("{}", Property::Integer(42)), "42");
    }

    #[test]
    fn display_list() {
        let list = Property::List(vec![
            Property::Integer(1),
            Property::String("two".to_string()),
        ]);
        assert_eq!(format!("{}", list), "[1, 'two']");
    }

    #[test]
    fn display_map() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), Property::Integer(1));
        m.insert("b".to_string(), Property::Integer(2));
        let map = Property::Map(m);
        let s = format!("{}", map);
        assert!(s.contains("a: 1"));
        assert!(s.contains("b: 2"));
    }

    #[test]
    fn property_discriminant_order() {
        let variants = [
            Property::Null,
            Property::Boolean(false),
            Property::Integer(0),
            Property::Float(OrderedF64(0.0)),
            Property::String("".to_string()),
            Property::List(vec![]),
            Property::Map(HashMap::new()),
            Property::Point { srid: 4326, x: OrderedF64(0.0), y: OrderedF64(0.0), z: None },
            Property::Date(0),
            Property::Duration(0),
        ];
        for i in 1..variants.len() {
            assert!(
                variants[i - 1].discriminant_ord() < variants[i].discriminant_ord(),
                "discriminant order violated at index {}",
                i
            );
        }
    }
}
