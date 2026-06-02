//! Order-preserving serialization codec for property values.
//!
//! This module provides a bijective encoding from [`Value`] to byte strings
//! that preserves **total lexicographic ordering**.  The ordering guarantees
//! are required by the property index B+ tree so that range scans on
//! `WHERE n.prop > val` and `WHERE n.prop BETWEEN a AND b` return correct
//! results without post-filtering.
//!
//! # Ordering guarantees
//!
//! ```text
//! NULL < false < true < i64::MIN < ... < i64::MAX
//!     < f64::NEG_INFINITY < ... < f64::NAN
//!     < "" < "a" < "ab" < ...
//!     < List([]) < List([...]) < Map({})
//! ```
//!
//! # Layout
//!
//! Every encoded value starts with a **type tag** (1 byte).  The remaining
//! bytes are type-specific and are also self-terminating so that concatenated
//! values can be parsed unambiguously.

use crate::graph::record::ValueType;

/// Maximum encoded length of a single value (generous upper bound).
pub const MAX_ENCODED_LEN: usize = 512;

/// A property value that can be encoded.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    // Floats are stored via their ordered bit-pattern; NaN is canonicalised
    // to a single representation and ordered after +inf.
    Float64(u64), // ordered bit-pattern, not raw f64
    String(Vec<u8>),
    List(Vec<u8>), // opaque payload for now
    Map(Vec<u8>),  // opaque payload for now
}

impl Value {
    /// Create a `Value::String` from a `&str`.
    pub fn from_str(s: &str) -> Self {
        Value::String(s.as_bytes().to_vec())
    }

    /// Create a `Value::Int64`.
    pub fn from_i64(v: i64) -> Self {
        Value::Int64(v)
    }

    /// Create a `Value::Float64` from an `f64`.
    ///
    /// NaN is canonicalised to the quiet-NAN bit pattern `0x7FF8000000000000`
    /// so that all NaNs compare equal and sort after +inf.
    pub fn from_f64(v: f64) -> Self {
        let bits = if v.is_nan() {
            0x7FF8_0000_0000_0000u64
        } else {
            v.to_bits()
        };
        Value::Float64(order_f64_bits(bits))
    }

    /// Recover the original `f64` from a `Value::Float64`.
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Value::Float64(ordered) => Some(f64::from_bits(unorder_f64_bits(*ordered))),
            _ => None,
        }
    }
}

/// Encode `value` into `out`, appending bytes.
///
/// Returns the number of bytes appended, or `None` if the encoded form
/// would exceed [`MAX_ENCODED_LEN`].
pub fn encode(value: &Value, out: &mut Vec<u8>) -> Option<usize> {
    let start = out.len();
    match value {
        Value::Null => {
            out.push(Tag::Null as u8);
        }
        Value::Bool(b) => {
            out.push(Tag::Bool as u8);
            out.push(if *b { 0x01 } else { 0x00 });
        }
        Value::Int64(v) => {
            out.push(Tag::Int64 as u8);
            out.extend_from_slice(&order_i64(*v).to_be_bytes());
        }
        Value::Float64(ordered) => {
            out.push(Tag::Float64 as u8);
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::String(bytes) => {
            out.push(Tag::String as u8);
            // Null-terminated with escaping to preserve lexicographic ordering:
            //   0x00 -> 0x00 0xFF
            //   0xFF -> 0xFF 0x00
            // Terminator is a bare 0x00 byte.
            for &b in bytes.iter().take(u16::MAX as usize) {
                if b == 0x00 {
                    out.extend_from_slice(&[0x00, 0xFF]);
                } else if b == 0xFF {
                    out.extend_from_slice(&[0xFF, 0x00]);
                } else {
                    out.push(b);
                }
            }
            out.push(0x00); // terminator
        }
        Value::List(bytes) => {
            out.push(Tag::List as u8);
            for &b in bytes.iter().take(u16::MAX as usize) {
                if b == 0x00 {
                    out.extend_from_slice(&[0x00, 0xFF]);
                } else if b == 0xFF {
                    out.extend_from_slice(&[0xFF, 0x00]);
                } else {
                    out.push(b);
                }
            }
            out.push(0x00);
        }
        Value::Map(bytes) => {
            out.push(Tag::Map as u8);
            for &b in bytes.iter().take(u16::MAX as usize) {
                if b == 0x00 {
                    out.extend_from_slice(&[0x00, 0xFF]);
                } else if b == 0xFF {
                    out.extend_from_slice(&[0xFF, 0x00]);
                } else {
                    out.push(b);
                }
            }
            out.push(0x00);
        }
    }
    let appended = out.len() - start;
    if appended > MAX_ENCODED_LEN {
        out.truncate(start);
        None
    } else {
        Some(appended)
    }
}

/// Decode the first value in `data`.
///
/// Returns `Some((value, bytes_consumed))` on success.
pub fn decode(data: &[u8]) -> Option<(Value, usize)> {
    if data.is_empty() {
        return None;
    }
    let tag = data[0];
    let mut off = 1;
    match tag {
        t if t == Tag::Null as u8 => Some((Value::Null, off)),
        t if t == Tag::Bool as u8 => {
            if data.len() < off + 1 {
                return None;
            }
            let b = data[off] != 0;
            off += 1;
            Some((Value::Bool(b), off))
        }
        t if t == Tag::Int64 as u8 => {
            if data.len() < off + 8 {
                return None;
            }
            let bits = u64::from_be_bytes([
                data[off], data[off + 1], data[off + 2], data[off + 3],
                data[off + 4], data[off + 5], data[off + 6], data[off + 7],
            ]);
            off += 8;
            Some((Value::Int64(unorder_i64(bits)), off))
        }
        t if t == Tag::Float64 as u8 => {
            if data.len() < off + 8 {
                return None;
            }
            let bits = u64::from_be_bytes([
                data[off], data[off + 1], data[off + 2], data[off + 3],
                data[off + 4], data[off + 5], data[off + 6], data[off + 7],
            ]);
            off += 8;
            Some((Value::Float64(bits), off))
        }
        t if t == Tag::String as u8 => {
            let (bytes, consumed) = decode_terminated(&data[off..])?;
            off += consumed;
            Some((Value::String(bytes), off))
        }
        t if t == Tag::List as u8 => {
            let (bytes, consumed) = decode_terminated(&data[off..])?;
            off += consumed;
            Some((Value::List(bytes), off))
        }
        t if t == Tag::Map as u8 => {
            let (bytes, consumed) = decode_terminated(&data[off..])?;
            off += consumed;
            Some((Value::Map(bytes), off))
        }
        _ => None,
    }
}

/// Encode a `ValueType` + raw payload (as stored in a [`PropertyRecord`])
/// into an order-preserving byte string suitable for use as the
/// *value* portion of a property index key.
///
/// The caller must still prepend the property_id and entity_id to form
/// the full composite key.
pub fn encode_property_value(value_type: ValueType, payload: &[u8]) -> Option<Vec<u8>> {
    let value = match value_type {
        ValueType::Null => Value::Null,
        ValueType::Bool => {
            if payload.is_empty() {
                return None;
            }
            Value::Bool(payload[0] != 0)
        }
        ValueType::Int64 => {
            if payload.len() < 8 {
                return None;
            }
            let v = i64::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
                payload[4], payload[5], payload[6], payload[7],
            ]);
            Value::Int64(v)
        }
        ValueType::Float64 => {
            if payload.len() < 8 {
                return None;
            }
            let bits = u64::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
                payload[4], payload[5], payload[6], payload[7],
            ]);
            Value::Float64(order_f64_bits(bits))
        }
        ValueType::String => Value::String(payload.to_vec()),
        ValueType::List => Value::List(payload.to_vec()),
        ValueType::Map => Value::Map(payload.to_vec()),
    };
    let mut buf = Vec::new();
    encode(&value, &mut buf)?;
    Some(buf)
}

// ------------------------------------------------------------------
// Internal helpers
// ------------------------------------------------------------------

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Null = 0x00,
    Bool = 0x01,
    Int64 = 0x02,
    Float64 = 0x03,
    String = 0x04,
    List = 0x05,
    Map = 0x06,
}

/// Reinterpret an `i64` as a `u64` so that lexicographic byte order matches
/// numeric order.
///
/// Technique: flip the sign bit.  Negative numbers then sort before
/// positive numbers because their most-significant byte is `0x00..0x7F`
/// while positive numbers start at `0x80..0xFF`.
fn order_i64(v: i64) -> u64 {
    (v as u64) ^ 0x8000_0000_0000_0000
}

/// Reverse [`order_i64`].
fn unorder_i64(v: u64) -> i64 {
    (v ^ 0x8000_0000_0000_0000) as i64
}

/// Reorder IEEE-754 `f64` bits so that lexicographic order matches
/// numeric order (negative < -0 < +0 < positive < NaN).
///
/// Algorithm:
/// * If sign bit is set (negative), flip **all** bits.
/// * If sign bit is clear (positive), flip **only** the sign bit.
fn order_f64_bits(bits: u64) -> u64 {
    if bits & 0x8000_0000_0000_0000 != 0 {
        // Negative: flip all bits.  This maps -inf -> 0x000... and -0 -> 0x7FF...
        !bits
    } else {
        // Positive: flip sign bit so that +0 -> 0x800... and +inf -> 0xFFF...
        bits ^ 0x8000_0000_0000_0000
    }
}

/// Reverse [`order_f64_bits`].
fn unorder_f64_bits(bits: u64) -> u64 {
    if bits & 0x8000_0000_0000_0000 != 0 {
        // Was positive: flip sign bit back.
        bits ^ 0x8000_0000_0000_0000
    } else {
        // Was negative: flip all bits back.
        !bits
    }
}

/// Decode a null-terminated byte sequence with escaping.
///
/// Returns `(decoded_bytes, total_consumed_bytes)`.
fn decode_terminated(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        // Check escapes first (look-ahead).
        if b == 0x00 && i + 1 < data.len() && data[i + 1] == 0xFF {
            // Escaped 0x00.
            out.push(0x00);
            i += 2;
            continue;
        }
        if b == 0xFF && i + 1 < data.len() && data[i + 1] == 0x00 {
            // Escaped 0xFF.
            out.push(0xFF);
            i += 2;
            continue;
        }
        if b == 0x00 {
            // Bare 0x00 is the terminator.
            return Some((out, i + 1));
        }
        out.push(b);
        i += 1;
    }
    // Reached end of data without finding terminator.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: &Value) {
        let mut buf = Vec::new();
        let n = encode(v, &mut buf).expect("encode succeeded");
        let (decoded, consumed) = decode(&buf).expect("decode succeeded");
        assert_eq!(consumed, n, "consumed bytes must match encoded length");
        assert_eq!(decoded, *v, "round-trip failed");
    }

    #[test]
    fn null_roundtrip() {
        roundtrip(&Value::Null);
    }

    #[test]
    fn bool_roundtrip() {
        roundtrip(&Value::Bool(false));
        roundtrip(&Value::Bool(true));
    }

    #[test]
    fn i64_roundtrip() {
        roundtrip(&Value::Int64(0));
        roundtrip(&Value::Int64(i64::MAX));
        roundtrip(&Value::Int64(i64::MIN));
        roundtrip(&Value::Int64(42));
        roundtrip(&Value::Int64(-42));
    }

    #[test]
    fn f64_roundtrip() {
        roundtrip(&Value::from_f64(0.0));
        roundtrip(&Value::from_f64(-0.0));
        roundtrip(&Value::from_f64(1.5));
        roundtrip(&Value::from_f64(-1.5));
        roundtrip(&Value::from_f64(f64::MAX));
        roundtrip(&Value::from_f64(f64::MIN));
        roundtrip(&Value::from_f64(f64::INFINITY));
        roundtrip(&Value::from_f64(f64::NEG_INFINITY));
        roundtrip(&Value::from_f64(f64::NAN));
    }

    #[test]
    fn string_roundtrip() {
        roundtrip(&Value::from_str(""));
        roundtrip(&Value::from_str("hello"));
        roundtrip(&Value::from_str("🚀 unicode"));
    }

    #[test]
    fn list_and_map_roundtrip() {
        roundtrip(&Value::List(vec![1, 2, 3]));
        roundtrip(&Value::Map(vec![0xAB, 0xCD]));
    }

    #[test]
    fn null_sorts_first() {
        let mut null_buf = Vec::new();
        encode(&Value::Null, &mut null_buf);
        let mut bool_buf = Vec::new();
        encode(&Value::Bool(false), &mut bool_buf);
        assert!(null_buf < bool_buf, "NULL must sort before Bool");
    }

    #[test]
    fn bool_ordering() {
        let mut false_buf = Vec::new();
        encode(&Value::Bool(false), &mut false_buf);
        let mut true_buf = Vec::new();
        encode(&Value::Bool(true), &mut true_buf);
        assert!(false_buf < true_buf, "false must sort before true");
    }

    #[test]
    fn i64_ordering() {
        let values = [i64::MIN, -1000i64, -1i64, 0i64, 1i64, 1000i64, i64::MAX];
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for v in values {
            let mut buf = Vec::new();
            encode(&Value::Int64(v), &mut buf);
            encoded.push(buf);
        }
        for i in 1..encoded.len() {
            assert!(
                encoded[i - 1] < encoded[i],
                "i64 ordering violated at index {}: {:?} vs {:?}",
                i,
                values[i - 1],
                values[i]
            );
        }
    }

    #[test]
    fn f64_ordering() {
        let values = [
            f64::NEG_INFINITY,
            -1000.0,
            -1.0,
            -0.0,
            0.0,
            1.0,
            1000.0,
            f64::INFINITY,
            f64::NAN,
        ];
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for v in values {
            let mut buf = Vec::new();
            encode(&Value::from_f64(v), &mut buf);
            encoded.push(buf);
        }
        for i in 1..encoded.len() {
            assert!(
                encoded[i - 1] < encoded[i],
                "f64 ordering violated at index {}: {:?} vs {:?}",
                i,
                values[i - 1],
                values[i]
            );
        }
    }

    #[test]
    fn string_ordering() {
        let values = ["", "a", "ab", "b", "ba", "🚀"];
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for s in values {
            let mut buf = Vec::new();
            encode(&Value::from_str(s), &mut buf);
            encoded.push(buf);
        }
        for i in 1..encoded.len() {
            assert!(
                encoded[i - 1] < encoded[i],
                "string ordering violated at index {}",
                i
            );
        }
    }

    #[test]
    fn cross_type_ordering() {
        // Verify: Null < Bool < Int64 < Float64 < String < List < Map
        let mut null_buf = Vec::new();
        encode(&Value::Null, &mut null_buf);
        let mut bool_buf = Vec::new();
        encode(&Value::Bool(true), &mut bool_buf);
        let mut int_buf = Vec::new();
        encode(&Value::Int64(0), &mut int_buf);
        let mut float_buf = Vec::new();
        encode(&Value::from_f64(0.0), &mut float_buf);
        let mut string_buf = Vec::new();
        encode(&Value::from_str(""), &mut string_buf);
        let mut list_buf = Vec::new();
        encode(&Value::List(vec![]), &mut list_buf);
        let mut map_buf = Vec::new();
        encode(&Value::Map(vec![]), &mut map_buf);

        let all = [
            null_buf, bool_buf, int_buf, float_buf, string_buf, list_buf, map_buf,
        ];
        for i in 1..all.len() {
            assert!(all[i - 1] < all[i], "cross-type ordering violated at index {}", i);
        }
    }

    #[test]
    fn encode_property_value_from_payload() {
        let payload = 42i64.to_be_bytes();
        let encoded = encode_property_value(ValueType::Int64, &payload).unwrap();
        let (decoded, _) = decode(&encoded).unwrap();
        assert_eq!(decoded, Value::Int64(42));
    }

    #[test]
    fn encode_property_value_float_roundtrip() {
        let payload = 3.14f64.to_bits().to_be_bytes();
        let encoded = encode_property_value(ValueType::Float64, &payload).unwrap();
        let (decoded, _) = decode(&encoded).unwrap();
        assert_eq!(decoded.to_f64(), Some(3.14));
    }
}
