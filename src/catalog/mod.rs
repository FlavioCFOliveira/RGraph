//! Schema catalog — tracks all labels, relationship types, and property keys
//! defined in the graph, mapping each to a compact u32 id.
//!
//! The catalog is the single source of truth for schema identifiers.  Every
//! label, relationship type, and property key is registered here on first use
//! and subsequently referenced by its stable u32 id.  The id space is dense
//! starting at 1 (id 0 is reserved as a sentinel meaning "no schema element").
//!
//! # Persistence
//!
//! The catalog is persisted to a dedicated sidecar file that lives next to the
//! database data file (`<data>.catalog`), mirroring the double-write sidecar
//! (`<data>.dw`).  On [`GraphStorageEngine::sync`] the in-memory snapshot is
//! serialised and written crash-safely (write to a temporary file, `fsync`,
//! then atomic `rename` over the final path).  On
//! [`GraphStorageEngine::open`] the sidecar is read back and the name → id
//! maps are restored, so labelled scans (`MATCH (n:Person)`) resolve correctly
//! after a restart.
//!
//! The sidecar payload is framed with a magic number, a CRC32C checksum, and a
//! length prefix.  A torn or corrupt sidecar fails the integrity check and is
//! ignored (the catalog falls back to empty, exactly as before this file
//! existed), so a partial write can never corrupt schema resolution — it only
//! costs the (recoverable) name → id mapping for that session.
//!
//! [`GraphStorageEngine::sync`]: crate::graph::engine::GraphStorageEngine::sync
//! [`GraphStorageEngine::open`]: crate::graph::engine::GraphStorageEngine::open
//!
//! # Thread safety
//!
//! The catalog does not provide its own locks.  Callers that need concurrent
//! access should wrap it in an `Arc<RwLock<Catalog>>` (the pattern used in
//! `GraphStorageEngine`).

use crate::io::FileSystem;
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// Magic signature for the catalog sidecar payload (`"RGCATLG\0"`).
const CATALOG_MAGIC: u64 = 0x5247_4341_544C_4700;

/// On-disk sidecar header: magic (8) + crc32c (4) + body length (4) = 16 bytes.
const CATALOG_HEADER_LEN: usize = 16;

// ─────────────────────────────────────────────────────────────────────────────
// Catalog
// ─────────────────────────────────────────────────────────────────────────────

/// Schema catalog for a single graph database.
///
/// Maps human-readable names to compact u32 ids for labels, relationship
/// types, and property keys.  Id 0 is reserved; real ids start at 1.
///
/// # Examples
///
/// ```
/// use rgraph::catalog::Catalog;
///
/// let mut catalog = Catalog::new();
/// let label_id = catalog.get_or_create_label("Person");
/// assert_eq!(catalog.label_id("Person"), Some(label_id));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Label name → u32 id.
    labels: HashMap<String, u32>,
    /// Reverse: label id → name.
    labels_rev: HashMap<u32, String>,
    /// Next label id to assign.
    next_label_id: u32,

    /// Relationship-type name → u32 id.
    rel_types: HashMap<String, u32>,
    /// Reverse: rel_type id → name.
    rel_types_rev: HashMap<u32, String>,
    /// Next relationship-type id to assign.
    next_rel_type_id: u32,

    /// Property-key name → u32 id.
    property_keys: HashMap<String, u32>,
    /// Reverse: property_key id → name.
    property_keys_rev: HashMap<u32, String>,
    /// Next property-key id to assign.
    next_property_key_id: u32,
}

impl Catalog {
    /// Create an empty catalog (all id sequences start at 1).
    pub fn new() -> Self {
        Self {
            next_label_id: 1,
            next_rel_type_id: 1,
            next_property_key_id: 1,
            ..Default::default()
        }
    }

    // ── Labels ────────────────────────────────────────────────────────────

    /// Return the id for `label`, creating it if it does not yet exist.
    pub fn get_or_create_label(&mut self, label: &str) -> u32 {
        if let Some(&id) = self.labels.get(label) {
            return id;
        }
        let id = self.next_label_id;
        self.next_label_id += 1;
        self.labels.insert(label.to_string(), id);
        self.labels_rev.insert(id, label.to_string());
        id
    }

    /// Look up the id for an existing label.  Returns `None` if unknown.
    pub fn label_id(&self, label: &str) -> Option<u32> {
        self.labels.get(label).copied()
    }

    /// Look up the name for a label id.  Returns `None` if unknown.
    pub fn label_name(&self, id: u32) -> Option<&str> {
        self.labels_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered labels as `(name, id)` pairs.
    pub fn all_labels(&self) -> impl Iterator<Item = (&str, u32)> {
        self.labels.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Relationship types ────────────────────────────────────────────────

    /// Return the id for `rel_type`, creating it if it does not yet exist.
    pub fn get_or_create_rel_type(&mut self, rel_type: &str) -> u32 {
        if let Some(&id) = self.rel_types.get(rel_type) {
            return id;
        }
        let id = self.next_rel_type_id;
        self.next_rel_type_id += 1;
        self.rel_types.insert(rel_type.to_string(), id);
        self.rel_types_rev.insert(id, rel_type.to_string());
        id
    }

    /// Look up the id for an existing relationship type.  Returns `None` if unknown.
    pub fn rel_type_id(&self, rel_type: &str) -> Option<u32> {
        self.rel_types.get(rel_type).copied()
    }

    /// Look up the name for a relationship type id.  Returns `None` if unknown.
    pub fn rel_type_name(&self, id: u32) -> Option<&str> {
        self.rel_types_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered relationship types as `(name, id)` pairs.
    pub fn all_rel_types(&self) -> impl Iterator<Item = (&str, u32)> {
        self.rel_types.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Property keys ─────────────────────────────────────────────────────

    /// Return the id for `property_key`, creating it if it does not yet exist.
    pub fn get_or_create_property_key(&mut self, key: &str) -> u32 {
        if let Some(&id) = self.property_keys.get(key) {
            return id;
        }
        let id = self.next_property_key_id;
        self.next_property_key_id += 1;
        self.property_keys.insert(key.to_string(), id);
        self.property_keys_rev.insert(id, key.to_string());
        id
    }

    /// Look up the id for an existing property key.  Returns `None` if unknown.
    pub fn property_key_id(&self, key: &str) -> Option<u32> {
        self.property_keys.get(key).copied()
    }

    /// Look up the name for a property key id.  Returns `None` if unknown.
    pub fn property_key_name(&self, id: u32) -> Option<&str> {
        self.property_keys_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered property keys as `(name, id)` pairs.
    pub fn all_property_keys(&self) -> impl Iterator<Item = (&str, u32)> {
        self.property_keys.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Serialisation ─────────────────────────────────────────────────────

    /// Encode the catalog to a byte vector (little-endian, length-prefixed).
    ///
    /// Format per section (labels, rel_types, property_keys):
    /// ```text
    /// count    : u32 LE
    /// [entry]* : (id: u32 LE, name_len: u16 LE, name: UTF-8 bytes)
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_section(&mut buf, &self.labels);
        encode_section(&mut buf, &self.rel_types);
        encode_section(&mut buf, &self.property_keys);
        buf
    }

    /// Decode a catalog from bytes produced by [`encode`].
    ///
    /// # Errors
    ///
    /// Returns `None` if the bytes are truncated or malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let (labels, labels_rev, next_label_id) = decode_section(bytes, &mut pos)?;
        let (rel_types, rel_types_rev, next_rel_type_id) = decode_section(bytes, &mut pos)?;
        let (property_keys, property_keys_rev, next_property_key_id) =
            decode_section(bytes, &mut pos)?;
        Some(Self {
            labels,
            labels_rev,
            next_label_id,
            rel_types,
            rel_types_rev,
            next_rel_type_id,
            property_keys,
            property_keys_rev,
            next_property_key_id,
        })
    }

    // ── Sidecar persistence ───────────────────────────────────────────────

    /// Frame the catalog snapshot with a magic number, CRC32C checksum, and a
    /// length prefix, ready to be written to the sidecar file.
    ///
    /// Layout: `magic (u64 LE) | crc32c(body) (u32 LE) | body_len (u32 LE) | body`.
    fn frame(&self) -> Vec<u8> {
        let body = self.encode();
        let crc = crc32c::crc32c(&body);
        let mut framed = Vec::with_capacity(CATALOG_HEADER_LEN + body.len());
        framed.extend_from_slice(&CATALOG_MAGIC.to_le_bytes());
        framed.extend_from_slice(&crc.to_le_bytes());
        framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
        framed.extend_from_slice(&body);
        framed
    }

    /// Parse a framed sidecar payload, verifying the magic, length, and CRC.
    ///
    /// Returns `None` if the frame is truncated, has the wrong magic, fails its
    /// checksum, or its body cannot be decoded — in every such case the caller
    /// falls back to an empty catalog rather than trusting corrupt data.
    fn unframe(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CATALOG_HEADER_LEN {
            return None;
        }
        let magic = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        if magic != CATALOG_MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let body_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
        let body = bytes.get(CATALOG_HEADER_LEN..CATALOG_HEADER_LEN + body_len)?;
        if crc32c::crc32c(body) != crc {
            return None;
        }
        Self::decode(body)
    }

    /// Persist the catalog crash-safely to the sidecar at `path`.
    ///
    /// Writes the framed snapshot to a sibling temporary file, flushes it, and
    /// atomically renames it over `path`.  A crash before the rename leaves the
    /// previous (or absent) sidecar intact; a crash after the rename leaves the
    /// new sidecar fully present — there is no torn intermediate state visible
    /// at `path`.
    ///
    /// # Errors
    ///
    /// Propagates any I/O failure from the underlying file system.
    pub fn persist(&self, path: &Path, fs: &dyn FileSystem) -> io::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NONCE: AtomicU64 = AtomicU64::new(0);

        let framed = self.frame();

        // Unique temp name in the same directory: the rename stays atomic (same
        // filesystem) and two concurrent persists never share — and thus never
        // corrupt — one another's temp file (finding M3).  pid distinguishes
        // processes; the monotonic nonce distinguishes calls within a process.
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "catalog".to_string());
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dir.join(format!(".{file_name}.{}.{nonce}.tmp", std::process::id()));

        {
            let handle = fs.open(&tmp_path, true)?;
            handle.set_len(0)?;
            handle.write_at(&framed, 0)?;
            handle.sync_all()?;
        }
        fs.rename(&tmp_path, path)?;

        // POSIX: the new directory entry created by the rename is only durable
        // after the parent directory itself is fsync'd.  Without this, a crash
        // after the rename can lose the catalog entirely — labelled MATCH then
        // returns zero rows (finding M3).
        fs.sync_dir(dir)?;
        Ok(())
    }

    /// Load a catalog from the sidecar at `path`, if it exists and is valid.
    ///
    /// Returns:
    /// * `Ok(Some(catalog))` when a valid sidecar is present.
    /// * `Ok(None)` when the sidecar is absent, empty, truncated, or fails its
    ///   integrity check — the caller should start from an empty catalog.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures other than "file not found" from opening or
    /// reading the sidecar.
    pub fn load(path: &Path, fs: &dyn FileSystem) -> io::Result<Option<Self>> {
        if !fs.exists(path) {
            return Ok(None);
        }
        let handle = fs.open(path, false)?;
        let len = handle.len()? as usize;
        if len == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        handle.read_at(&mut buf, 0)?;
        Ok(Self::unframe(&buf))
    }
}

fn encode_section(buf: &mut Vec<u8>, map: &HashMap<String, u32>) {
    buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
    for (name, &id) in map {
        buf.extend_from_slice(&id.to_le_bytes());
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
}

/// A decoded catalog section: the forward `name -> id` map, the reverse
/// `id -> name` map, and the next free id.
type DecodedSection = (HashMap<String, u32>, HashMap<u32, String>, u32);

fn decode_section(bytes: &[u8], pos: &mut usize) -> Option<DecodedSection> {
    let count = read_u32_le(bytes, pos)? as usize;
    let mut map = HashMap::with_capacity(count);
    let mut rev = HashMap::with_capacity(count);
    let mut max_id = 0u32;
    for _ in 0..count {
        let id = read_u32_le(bytes, pos)?;
        let name_len = read_u16_le(bytes, pos)? as usize;
        if *pos + name_len > bytes.len() {
            return None;
        }
        let name = std::str::from_utf8(&bytes[*pos..*pos + name_len])
            .ok()?
            .to_string();
        *pos += name_len;
        if id > max_id {
            max_id = id;
        }
        map.insert(name.clone(), id);
        rev.insert(id, name);
    }
    Some((map, rev, max_id + 1))
}

fn read_u32_le(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > bytes.len() {
        return None;
    }
    let val = u32::from_le_bytes([
        bytes[*pos],
        bytes[*pos + 1],
        bytes[*pos + 2],
        bytes[*pos + 3],
    ]);
    *pos += 4;
    Some(val)
}

fn read_u16_le(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos + 2 > bytes.len() {
        return None;
    }
    let val = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
    *pos += 2;
    Some(val)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_get_or_create_idempotent() {
        let mut c = Catalog::new();
        let id1 = c.get_or_create_label("Person");
        let id2 = c.get_or_create_label("Person");
        assert_eq!(id1, id2);
        assert_ne!(id1, 0, "id 0 is reserved");
    }

    #[test]
    fn distinct_labels_get_distinct_ids() {
        let mut c = Catalog::new();
        let person = c.get_or_create_label("Person");
        let movie = c.get_or_create_label("Movie");
        assert_ne!(person, movie);
    }

    #[test]
    fn label_id_returns_none_for_unknown() {
        let c = Catalog::new();
        assert_eq!(c.label_id("Unknown"), None);
    }

    #[test]
    fn label_name_roundtrip() {
        let mut c = Catalog::new();
        let id = c.get_or_create_label("Actor");
        assert_eq!(c.label_name(id), Some("Actor"));
    }

    #[test]
    fn rel_type_registration() {
        let mut c = Catalog::new();
        let id = c.get_or_create_rel_type("KNOWS");
        assert_eq!(c.rel_type_id("KNOWS"), Some(id));
        assert_eq!(c.rel_type_name(id), Some("KNOWS"));
    }

    #[test]
    fn property_key_registration() {
        let mut c = Catalog::new();
        let id = c.get_or_create_property_key("name");
        assert_eq!(c.property_key_id("name"), Some(id));
        assert_eq!(c.property_key_name(id), Some("name"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut c = Catalog::new();
        c.get_or_create_label("Person");
        c.get_or_create_label("Movie");
        c.get_or_create_rel_type("KNOWS");
        c.get_or_create_property_key("name");
        c.get_or_create_property_key("age");

        let encoded = c.encode();
        let decoded = Catalog::decode(&encoded).expect("decode should succeed");

        assert_eq!(decoded.label_id("Person"), c.label_id("Person"));
        assert_eq!(decoded.label_id("Movie"), c.label_id("Movie"));
        assert_eq!(decoded.rel_type_id("KNOWS"), c.rel_type_id("KNOWS"));
        assert_eq!(decoded.property_key_id("name"), c.property_key_id("name"));
        assert_eq!(decoded.property_key_id("age"), c.property_key_id("age"));
    }

    #[test]
    fn new_ids_after_decode_do_not_collide() {
        let mut c = Catalog::new();
        c.get_or_create_label("A");
        let encoded = c.encode();
        let mut decoded = Catalog::decode(&encoded).unwrap();

        let new_id = decoded.get_or_create_label("B");
        assert!(
            new_id > decoded.label_id("A").unwrap(),
            "new id must be greater than any existing id"
        );
    }

    // ── Sidecar persistence (Task 189) ────────────────────────────────────

    #[test]
    fn frame_unframe_roundtrip() {
        let mut c = Catalog::new();
        c.get_or_create_label("Person");
        c.get_or_create_rel_type("KNOWS");
        c.get_or_create_property_key("name");

        let framed = c.frame();
        let back = Catalog::unframe(&framed).expect("valid frame must unframe");
        assert_eq!(back.label_id("Person"), c.label_id("Person"));
        assert_eq!(back.rel_type_id("KNOWS"), c.rel_type_id("KNOWS"));
        assert_eq!(back.property_key_id("name"), c.property_key_id("name"));
    }

    #[test]
    fn unframe_rejects_corrupt_payload() {
        let mut c = Catalog::new();
        c.get_or_create_label("Person");
        let mut framed = c.frame();
        // Flip a byte in the body — the CRC must reject it.
        let last = framed.len() - 1;
        framed[last] ^= 0xFF;
        assert!(
            Catalog::unframe(&framed).is_none(),
            "a corrupt body must fail the checksum and be ignored"
        );
    }

    #[test]
    fn unframe_rejects_wrong_magic() {
        let c = Catalog::new();
        let mut framed = c.frame();
        framed[0] ^= 0xFF; // corrupt the magic
        assert!(Catalog::unframe(&framed).is_none());
    }

    #[test]
    fn unframe_rejects_truncated() {
        let mut c = Catalog::new();
        c.get_or_create_label("Person");
        let framed = c.frame();
        // Drop the last few body bytes.
        assert!(Catalog::unframe(&framed[..framed.len() - 3]).is_none());
    }

    #[test]
    fn persist_then_load_roundtrip() {
        use crate::io::posix::PosixFileSystem;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db.catalog");
        let fs = PosixFileSystem::new(false);

        let mut c = Catalog::new();
        let person = c.get_or_create_label("Person");
        let movie = c.get_or_create_label("Movie");
        let knows = c.get_or_create_rel_type("KNOWS");
        let name = c.get_or_create_property_key("name");

        c.persist(&path, &fs).expect("persist must succeed");

        let loaded = Catalog::load(&path, &fs)
            .expect("load io must succeed")
            .expect("a valid sidecar must be present");

        assert_eq!(loaded.label_id("Person"), Some(person));
        assert_eq!(loaded.label_id("Movie"), Some(movie));
        assert_eq!(loaded.rel_type_id("KNOWS"), Some(knows));
        assert_eq!(loaded.property_key_id("name"), Some(name));
        assert_eq!(loaded.label_name(person), Some("Person"));
    }

    #[test]
    fn load_absent_sidecar_returns_none() {
        use crate::io::posix::PosixFileSystem;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.catalog");
        let fs = PosixFileSystem::new(false);
        assert!(Catalog::load(&path, &fs).unwrap().is_none());
    }

    #[test]
    fn persist_surfaces_directory_fsync_failure() {
        // Regression gate for finding M3 (2026-06-05): persist must fsync the
        // parent directory after the atomic rename and surface that failure,
        // so a crash after rename cannot silently lose the catalog.
        use crate::io::fault::{FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, OpMask};
        use crate::io::posix::PosixFileSystem;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db.catalog");
        let fs = FaultInjectFileSystem::new(Box::new(PosixFileSystem::new(false)));

        let mut c = Catalog::new();
        c.get_or_create_label("Person");

        // Fail ONLY the directory fsync (the temp file's own sync_all is untouched).
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    sync_dir: true,
                    ..OpMask::default()
                },
                kind: FaultKind::FsyncFail,
                every_n: None,
            }],
        });
        assert!(
            c.persist(&path, &fs).is_err(),
            "a directory fsync failure must be surfaced, not swallowed"
        );

        // With faults cleared, persist succeeds and the catalog is durable.
        fs.set_config(FaultConfig::default());
        c.persist(&path, &fs).unwrap();
        assert!(Catalog::load(&path, &fs).unwrap().is_some());
    }

    #[test]
    fn failed_persist_never_clobbers_the_live_catalog() {
        // Regression gate for finding M3 (2026-06-05): a persist that fails
        // before the rename must leave the existing catalog intact (atomic
        // rename) and must not reuse the old fixed temp name.
        use crate::io::fault::{FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, OpMask};
        use crate::io::posix::PosixFileSystem;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db.catalog");
        let fs = FaultInjectFileSystem::new(Box::new(PosixFileSystem::new(false)));

        // A good catalog is persisted first.
        let mut good = Catalog::new();
        let person = good.get_or_create_label("Person");
        good.persist(&path, &fs).unwrap();

        // A second persist whose temp write fails before the rename.
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    write: true,
                    ..OpMask::default()
                },
                kind: FaultKind::Eio,
                every_n: None,
            }],
        });
        let mut other = Catalog::new();
        other.get_or_create_label("Movie");
        assert!(other.persist(&path, &fs).is_err());

        // The original catalog must still be intact (never clobbered).
        fs.set_config(FaultConfig::default());
        let loaded = Catalog::load(&path, &fs).unwrap().unwrap();
        assert_eq!(loaded.label_id("Person"), Some(person));
        assert!(
            loaded.label_id("Movie").is_none(),
            "a failed persist must not leak into the live catalog"
        );

        // The old fixed temp name must never be used.
        assert!(
            !fs.exists(&path.with_extension("catalog.tmp")),
            "persist must use a unique temp name, not the fixed *.catalog.tmp"
        );
    }

    #[test]
    fn persist_overwrites_previous_sidecar() {
        use crate::io::posix::PosixFileSystem;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db.catalog");
        let fs = PosixFileSystem::new(false);

        let mut c1 = Catalog::new();
        c1.get_or_create_label("Person");
        c1.persist(&path, &fs).unwrap();

        let mut c2 = Catalog::new();
        c2.get_or_create_label("Person");
        let movie = c2.get_or_create_label("Movie");
        c2.persist(&path, &fs).unwrap();

        let loaded = Catalog::load(&path, &fs).unwrap().unwrap();
        assert_eq!(
            loaded.label_id("Movie"),
            Some(movie),
            "the newest snapshot must win after re-persist"
        );
    }
}
