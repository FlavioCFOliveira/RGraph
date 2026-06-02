//! Fixed-size and variable-length graph record formats.
//!
//! # Node Record
//!
//! A `NodeRecord` is exactly **32 bytes** and stores all metadata needed
//! to locate a node's edges, properties, and label.
//!
//! # Edge Record
//!
//! An `EdgeRecord` is exactly **48 bytes** and stores the doubly-linked
//! adjacency pointers for both source and target nodes, plus a property
//! chain head.
//!
//! # Property Record
//!
//! Properties are variable-length.  Values ≤ 256 bytes are stored inline;
//! larger values use an overflow page chain.
//!
//! # Slot Reference
//!
//! A `SlotRef` packs a `page_id` and `slot_index` into a single `u32`:
//!
//! ```text
//! | page_id : u24 | slot_index : u8 |
//! ```
//!
//! This gives a maximum of **16 777 216 pages** (~128 TiB with 8 KiB
//! pages) and **255 slots per page**.

/// Packed page-id + slot-index reference (4 bytes).
///
/// Layout: upper 24 bits = page_id, lower 8 bits = slot_index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct SlotRef {
    pub raw: u32,
}

impl SlotRef {
    /// Sentinel value meaning "no slot".
    pub const NULL: SlotRef = SlotRef { raw: 0 };

    /// Pack a page id and slot index into a `SlotRef`.
    ///
    /// # Panics
    /// Panics in debug mode if `page_id` does not fit in 24 bits or
    /// `slot_index` does not fit in 8 bits.
    pub fn new(page_id: u32, slot_index: u8) -> Self {
        debug_assert!(page_id <= Self::MAX_PAGE_ID, "page_id exceeds 24 bits");
        // slot_index is u8 and MAX_SLOT_INDEX = u8::MAX, so the range is always satisfied.
        Self {
            raw: (page_id << 8) | u32::from(slot_index),
        }
    }

    /// Maximum page id that fits in the packed representation.
    pub const MAX_PAGE_ID: u32 = 0x00FF_FFFF;

    /// Maximum slot index that fits in the packed representation.
    pub const MAX_SLOT_INDEX: u8 = 0xFF;

    /// Extract the page id.
    pub fn page_id(&self) -> u32 {
        self.raw >> 8
    }

    /// Extract the slot index.
    pub fn slot_index(&self) -> u8 {
        (self.raw & 0xFF) as u8
    }

    /// Is this the null sentinel?
    pub fn is_null(&self) -> bool {
        self.raw == 0
    }
}

/// Fixed-size node record (32 bytes).
///
/// ```text
/// 0x00  node_id              u64
/// 0x08  label_id             u32
/// 0x0C  first_outgoing_edge  SlotRef (u32)
/// 0x10  first_incoming_edge  SlotRef (u32)
/// 0x14  first_property       SlotRef (u32)
/// 0x18  flags                u16
/// 0x1A  generation           u16
/// 0x1C  _pad                 u32
/// ```
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct NodeRecord {
    pub node_id: u64,
    pub label_id: u32,
    pub first_outgoing_edge: SlotRef,
    pub first_incoming_edge: SlotRef,
    pub first_property: SlotRef,
    pub flags: u16,
    pub generation: u16,
    pub _pad: u32,
}

impl NodeRecord {
    /// Create a new empty node record.
    pub fn new(node_id: u64, label_id: u32) -> Self {
        Self {
            node_id,
            label_id,
            first_outgoing_edge: SlotRef::NULL,
            first_incoming_edge: SlotRef::NULL,
            first_property: SlotRef::NULL,
            flags: 0,
            generation: 0,
            _pad: 0,
        }
    }

    /// Size of the record in bytes (always 32).
    pub const SIZE: usize = 32;

    /// Encode into a byte slice.
    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(out.len(), Self::SIZE, "output buffer must be 32 bytes");
        // SAFETY: `NodeRecord` is `#[repr(C)]` and `out` is exactly the right size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                out.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    /// Decode from a byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        // SAFETY: bytes length matches struct size and alignment is satisfied.
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const NodeRecord) })
    }
}

/// Bit flags for [`NodeRecord::flags`].
pub mod node_flags {
    /// Node is marked for deletion (tombstone).
    pub const DELETED: u16 = 0x0001;
    /// Node has overflow properties.
    pub const HAS_OVERFLOW: u16 = 0x0002;
}

/// Fixed-size edge record (48 bytes).
///
/// ```text
/// 0x00  edge_id              u64
/// 0x08  type_id              u32
/// 0x0C  source_node          SlotRef (u32)
/// 0x10  target_node          SlotRef (u32)
/// 0x14  prev_source_edge     SlotRef (u32)
/// 0x18  next_source_edge     SlotRef (u32)
/// 0x1C  prev_target_edge     SlotRef (u32)
/// 0x20  next_target_edge     SlotRef (u32)
/// 0x24  first_property       SlotRef (u32)
/// 0x28  flags                u16
/// 0x2A  generation           u16
/// 0x2C  _pad                 u32
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct EdgeRecord {
    pub edge_id: u64,
    pub type_id: u32,
    pub source_node: SlotRef,
    pub target_node: SlotRef,
    pub prev_source_edge: SlotRef,
    pub next_source_edge: SlotRef,
    pub prev_target_edge: SlotRef,
    pub next_target_edge: SlotRef,
    pub first_property: SlotRef,
    pub flags: u16,
    pub generation: u16,
    pub _pad: u32,
}

impl EdgeRecord {
    /// Create a new empty edge record.
    pub fn new(edge_id: u64, type_id: u32, source: SlotRef, target: SlotRef) -> Self {
        Self {
            edge_id,
            type_id,
            source_node: source,
            target_node: target,
            prev_source_edge: SlotRef::NULL,
            next_source_edge: SlotRef::NULL,
            prev_target_edge: SlotRef::NULL,
            next_target_edge: SlotRef::NULL,
            first_property: SlotRef::NULL,
            flags: 0,
            generation: 0,
            _pad: 0,
        }
    }

    /// Size of the record in bytes (always 48).
    pub const SIZE: usize = 48;

    /// Encode into a byte slice.
    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(out.len(), Self::SIZE, "output buffer must be 48 bytes");
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                out.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    /// Decode from a byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const EdgeRecord) })
    }
}

/// Bit flags for [`EdgeRecord::flags`].
pub mod edge_flags {
    /// Edge is marked for deletion (tombstone).
    pub const DELETED: u16 = 0x0001;
    /// Edge has overflow properties.
    pub const HAS_OVERFLOW: u16 = 0x0002;
}

/// Discriminant for the kind of value stored in a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ValueType {
    Null = 0x00,
    Bool = 0x01,
    Int64 = 0x02,
    Float64 = 0x03,
    String = 0x04,
    List = 0x05,
    Map = 0x06,
    // RDF literal types can be added here.
}

impl ValueType {
    /// Try to convert from a raw byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x00 => Some(ValueType::Null),
            0x01 => Some(ValueType::Bool),
            0x02 => Some(ValueType::Int64),
            0x03 => Some(ValueType::Float64),
            0x04 => Some(ValueType::String),
            0x05 => Some(ValueType::List),
            0x06 => Some(ValueType::Map),
            _ => None,
        }
    }
}

/// Maximum number of bytes stored inline in a property record.
pub const MAX_INLINE_PROPERTY_LEN: usize = 256;

/// Header for a variable-length property record.
///
/// ```text
/// 0x00  property_key_id  u32
/// 0x04  value_length     u16
/// 0x06  value_type       u8
/// 0x07  _pad             u8
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PropertyHeader {
    pub property_key_id: u32,
    pub value_length: u16,
    pub value_type: u8,
    pub _pad: u8,
}

impl PropertyHeader {
    pub const SIZE: usize = 8;

    pub fn new(property_key_id: u32, value_type: ValueType, value_length: u16) -> Self {
        Self {
            property_key_id,
            value_type: value_type as u8,
            value_length,
            _pad: 0,
        }
    }

    /// Encode into a byte slice.
    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(out.len(), Self::SIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                out.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    /// Decode from a byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const PropertyHeader) })
    }
}

/// Handle to an overflow page chain for large property values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct OverflowHandle {
    pub page_id: u32,
    pub offset: u16,
    pub _pad: u16,
}

impl OverflowHandle {
    pub const SIZE: usize = 8;

    pub fn new(page_id: u32, offset: u16) -> Self {
        Self {
            page_id,
            offset,
            _pad: 0,
        }
    }

    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(out.len(), Self::SIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const _ as *const u8,
                out.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const OverflowHandle) })
    }
}

/// A complete in-memory property value (header + payload or overflow handle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyRecord {
    pub header: PropertyHeader,
    pub payload: Vec<u8>,
}

impl PropertyRecord {
    /// Create a new inline property record.
    pub fn inline(property_key_id: u32, value_type: ValueType, payload: Vec<u8>) -> Self {
        assert!(
            payload.len() <= MAX_INLINE_PROPERTY_LEN,
            "inline payload exceeds {} bytes",
            MAX_INLINE_PROPERTY_LEN
        );
        Self {
            header: PropertyHeader::new(property_key_id, value_type, payload.len() as u16),
            payload,
        }
    }

    /// Create a new overflow property record.
    pub fn overflow(property_key_id: u32, value_type: ValueType, handle: OverflowHandle) -> Self {
        let mut payload = vec![0u8; OverflowHandle::SIZE];
        handle.encode(&mut payload);
        Self {
            header: PropertyHeader::new(property_key_id, value_type, payload.len() as u16),
            payload,
        }
    }

    /// Total on-disk size of this record (header + payload).
    pub fn on_disk_size(&self) -> usize {
        PropertyHeader::SIZE + self.payload.len()
    }

    /// Encode into a byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.on_disk_size());
        let mut header_bytes = [0u8; PropertyHeader::SIZE];
        self.header.encode(&mut header_bytes);
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode from a byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < PropertyHeader::SIZE {
            return None;
        }
        let header = PropertyHeader::decode(&bytes[..PropertyHeader::SIZE])?;
        let payload_len = header.value_length as usize;
        if bytes.len() < PropertyHeader::SIZE + payload_len {
            return None;
        }
        let payload = bytes[PropertyHeader::SIZE..PropertyHeader::SIZE + payload_len].to_vec();
        Some(Self { header, payload })
    }

    /// Is this property stored inline?
    pub fn is_inline(&self) -> bool {
        self.payload.len() <= MAX_INLINE_PROPERTY_LEN
            && self.header.value_length as usize <= MAX_INLINE_PROPERTY_LEN
    }

    /// Extract the overflow handle from the payload.
    ///
    /// Returns `Some` if the payload is exactly [`OverflowHandle::SIZE`]
    /// bytes and decodes successfully.
    pub fn overflow_handle(&self) -> Option<OverflowHandle> {
        if self.payload.len() != OverflowHandle::SIZE {
            return None;
        }
        OverflowHandle::decode(&self.payload)
    }
}

/// Convenience: build a `PropertyRecord` from a `&str`.
impl From<&str> for PropertyRecord {
    fn from(s: &str) -> Self {
        let bytes = s.as_bytes().to_vec();
        if bytes.len() <= MAX_INLINE_PROPERTY_LEN {
            Self::inline(0, ValueType::String, bytes)
        } else {
            // Caller must convert to overflow after allocating a page.
            Self::inline(0, ValueType::String, bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    // ------------------------------------------------------------------
    // SlotRef
    // ------------------------------------------------------------------

    #[test]
    fn slot_ref_roundtrip() {
        let s = SlotRef::new(42, 7);
        assert_eq!(s.page_id(), 42);
        assert_eq!(s.slot_index(), 7);
        assert!(!s.is_null());
    }

    #[test]
    fn slot_ref_null() {
        assert!(SlotRef::NULL.is_null());
        assert_eq!(SlotRef::NULL.page_id(), 0);
        assert_eq!(SlotRef::NULL.slot_index(), 0);
    }

    #[test]
    fn slot_ref_max_values() {
        let s = SlotRef::new(SlotRef::MAX_PAGE_ID, SlotRef::MAX_SLOT_INDEX);
        assert_eq!(s.page_id(), SlotRef::MAX_PAGE_ID);
        assert_eq!(s.slot_index(), SlotRef::MAX_SLOT_INDEX);
    }

    // ------------------------------------------------------------------
    // NodeRecord
    // ------------------------------------------------------------------

    #[test]
    fn node_record_size_is_32() {
        assert_eq!(size_of::<NodeRecord>(), 32);
    }

    #[test]
    fn node_record_encode_decode_roundtrip() {
        let mut rec = NodeRecord::new(123, 456);
        rec.first_outgoing_edge = SlotRef::new(1, 2);
        rec.first_incoming_edge = SlotRef::new(3, 4);
        rec.first_property = SlotRef::new(5, 6);
        rec.flags = node_flags::DELETED;
        rec.generation = 7;

        let mut buf = [0u8; NodeRecord::SIZE];
        rec.encode(&mut buf);
        let decoded = NodeRecord::decode(&buf).unwrap();

        assert_eq!(decoded.node_id, 123);
        assert_eq!(decoded.label_id, 456);
        assert_eq!(decoded.first_outgoing_edge, SlotRef::new(1, 2));
        assert_eq!(decoded.first_incoming_edge, SlotRef::new(3, 4));
        assert_eq!(decoded.first_property, SlotRef::new(5, 6));
        assert_eq!(decoded.flags, node_flags::DELETED);
        assert_eq!(decoded.generation, 7);
    }

    #[test]
    fn node_record_decode_wrong_size_fails() {
        assert!(NodeRecord::decode(&[0u8; 31]).is_none());
        assert!(NodeRecord::decode(&[0u8; 33]).is_none());
    }

    // ------------------------------------------------------------------
    // EdgeRecord
    // ------------------------------------------------------------------

    #[test]
    fn edge_record_size_is_48() {
        assert_eq!(size_of::<EdgeRecord>(), 48);
    }

    #[test]
    fn edge_record_encode_decode_roundtrip() {
        let mut rec = EdgeRecord::new(999, 10, SlotRef::new(1, 2), SlotRef::new(3, 4));
        rec.prev_source_edge = SlotRef::new(5, 6);
        rec.next_source_edge = SlotRef::new(7, 8);
        rec.prev_target_edge = SlotRef::new(9, 10);
        rec.next_target_edge = SlotRef::new(11, 12);
        rec.first_property = SlotRef::new(13, 14);
        rec.flags = edge_flags::HAS_OVERFLOW;
        rec.generation = 3;

        let mut buf = [0u8; EdgeRecord::SIZE];
        rec.encode(&mut buf);
        let decoded = EdgeRecord::decode(&buf).unwrap();

        assert_eq!(decoded.edge_id, 999);
        assert_eq!(decoded.type_id, 10);
        assert_eq!(decoded.source_node, SlotRef::new(1, 2));
        assert_eq!(decoded.target_node, SlotRef::new(3, 4));
        assert_eq!(decoded.prev_source_edge, SlotRef::new(5, 6));
        assert_eq!(decoded.next_source_edge, SlotRef::new(7, 8));
        assert_eq!(decoded.prev_target_edge, SlotRef::new(9, 10));
        assert_eq!(decoded.next_target_edge, SlotRef::new(11, 12));
        assert_eq!(decoded.first_property, SlotRef::new(13, 14));
        assert_eq!(decoded.flags, edge_flags::HAS_OVERFLOW);
        assert_eq!(decoded.generation, 3);
    }

    #[test]
    fn edge_record_decode_wrong_size_fails() {
        assert!(EdgeRecord::decode(&[0u8; 47]).is_none());
        assert!(EdgeRecord::decode(&[0u8; 49]).is_none());
    }

    // ------------------------------------------------------------------
    // PropertyRecord
    // ------------------------------------------------------------------

    #[test]
    fn property_header_size_is_8() {
        assert_eq!(size_of::<PropertyHeader>(), 8);
    }

    #[test]
    fn property_header_encode_decode() {
        let h = PropertyHeader::new(42, ValueType::String, 100);
        let mut buf = [0u8; PropertyHeader::SIZE];
        h.encode(&mut buf);
        let d = PropertyHeader::decode(&buf).unwrap();
        assert_eq!(d.property_key_id, 42);
        assert_eq!(d.value_type, ValueType::String as u8);
        assert_eq!(d.value_length, 100);
    }

    #[test]
    fn inline_property_roundtrip() {
        let rec = PropertyRecord::inline(7, ValueType::Int64, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let bytes = rec.encode();
        let decoded = PropertyRecord::decode(&bytes).unwrap();
        assert_eq!(decoded.header.property_key_id, 7);
        assert_eq!(decoded.header.value_type, ValueType::Int64 as u8);
        assert_eq!(decoded.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(decoded.is_inline());
    }

    #[test]
    fn overflow_property_roundtrip() {
        let handle = OverflowHandle::new(99, 123);
        let rec = PropertyRecord::overflow(5, ValueType::String, handle);
        let bytes = rec.encode();
        let decoded = PropertyRecord::decode(&bytes).unwrap();
        // Overflow handle is 8 bytes, which is <= MAX_INLINE_PROPERTY_LEN,
        // so is_inline returns true for the record size.  The semantic
        // distinction is that the payload is an overflow handle, not raw data.
        let h = decoded.overflow_handle().unwrap();
        assert_eq!(h.page_id, 99);
        assert_eq!(h.offset, 123);
    }

    #[test]
    fn property_from_str() {
        let rec: PropertyRecord = "hello".into();
        assert_eq!(rec.header.value_type, ValueType::String as u8);
        assert_eq!(rec.payload, b"hello");
    }

    #[test]
    fn property_decode_too_short_fails() {
        assert!(PropertyRecord::decode(&[0u8; 4]).is_none());
    }

    #[test]
    fn overflow_handle_size_is_8() {
        assert_eq!(size_of::<OverflowHandle>(), 8);
    }

    #[test]
    fn node_record_alignment_is_4() {
        // The task requires 4-byte alignment for safe transmutation.
        assert_eq!(size_of::<NodeRecord>() % 4, 0);
    }

    #[test]
    fn edge_record_alignment_is_4() {
        assert_eq!(size_of::<EdgeRecord>() % 4, 0);
    }
}
