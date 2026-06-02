//! Prefix compression for B+ tree pages.
//!
//! Reduces per-page key storage by extracting the longest common prefix
//! shared by all keys on a page and storing it once in the page header
//! area.  Each slot then stores only the suffix, typically cutting key
//! sizes by 30–50 % for RDF URI keys and adjacency keys.

/// Compute the longest common prefix of a slice of byte strings.
///
/// Returns an empty slice when `keys` is empty.
pub fn common_prefix(keys: &[&[u8]]) -> Vec<u8> {
    if keys.is_empty() {
        return Vec::new();
    }
    let first = keys[0];
    let mut prefix_len = first.len();
    for key in keys.iter().skip(1) {
        prefix_len = prefix_len.min(key.len());
        for i in 0..prefix_len {
            if first[i] != key[i] {
                prefix_len = i;
                break;
            }
        }
        if prefix_len == 0 {
            break;
        }
    }
    first[..prefix_len].to_vec()
}

/// Strip `prefix` from the front of `key`.
///
/// Returns the suffix bytes.  Panics in debug mode if `key` does not
/// actually start with `prefix`.
pub fn strip_prefix<'a>(key: &'a [u8], prefix: &[u8]) -> &'a [u8] {
    debug_assert!(key.starts_with(prefix), "key does not start with prefix");
    &key[prefix.len()..]
}

/// Rebuild a full key from a suffix record and the page prefix.
///
/// A *record* is `[key_len: u16 BE][suffix][value]`.  The function
/// prepends `prefix` to `suffix` and returns a new record containing
/// the full key.  Returns `None` if the record is malformed.
pub fn decompress_record(record: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if record.len() < 2 {
        return None;
    }
    let key_len = u16::from_be_bytes([record[0], record[1]]) as usize;
    if 2 + key_len > record.len() {
        return None;
    }
    let suffix = &record[2..2 + key_len];
    let value = &record[2 + key_len..];
    let full_key_len = prefix.len() + suffix.len();
    let mut out = Vec::with_capacity(2 + full_key_len + value.len());
    out.extend_from_slice(&(full_key_len as u16).to_be_bytes());
    out.extend_from_slice(prefix);
    out.extend_from_slice(suffix);
    out.extend_from_slice(value);
    Some(out)
}

/// Compress a record by replacing the full key with its suffix relative
/// to `prefix`.
///
/// Returns `None` if `key` does not start with `prefix`.
pub fn compress_record(record: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if record.len() < 2 {
        return None;
    }
    let key_len = u16::from_be_bytes([record[0], record[1]]) as usize;
    if 2 + key_len > record.len() {
        return None;
    }
    let key = &record[2..2 + key_len];
    if !key.starts_with(prefix) {
        return None;
    }
    let suffix = &key[prefix.len()..];
    let value = &record[2 + key_len..];
    let suffix_len = suffix.len();
    let mut out = Vec::with_capacity(2 + suffix_len + value.len());
    out.extend_from_slice(&(suffix_len as u16).to_be_bytes());
    out.extend_from_slice(suffix);
    out.extend_from_slice(value);
    Some(out)
}

/// Extract the key portion (without the value) from a record.
pub fn extract_key(record: &[u8]) -> Option<&[u8]> {
    if record.len() < 2 {
        return None;
    }
    let key_len = u16::from_be_bytes([record[0], record[1]]) as usize;
    if 2 + key_len > record.len() {
        return None;
    }
    Some(&record[2..2 + key_len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_prefix_empty() {
        assert_eq!(common_prefix(&[]), Vec::<u8>::new());
    }

    #[test]
    fn common_prefix_single() {
        assert_eq!(common_prefix(&[ b"hello" ]), b"hello");
    }

    #[test]
    fn common_prefix_multiple() {
        let keys: Vec<&[u8]> = vec![b"foobar", b"foobaz", b"foobat"];
        assert_eq!(common_prefix(&keys), b"fooba");
    }

    #[test]
    fn common_prefix_no_match() {
        let keys: Vec<&[u8]> = vec![b"abc", b"xyz"];
        assert_eq!(common_prefix(&keys), b"");
    }

    #[test]
    fn compress_decompress_roundtrip() {
        let prefix = b"http://example.org/";
        let key = b"http://example.org/node/42";
        let value = b"slot-ref";
        let mut record = Vec::new();
        record.extend_from_slice(&(key.len() as u16).to_be_bytes());
        record.extend_from_slice(key);
        record.extend_from_slice(value);

        let compressed = compress_record(&record, prefix).unwrap();
        let decompressed = decompress_record(&compressed, prefix).unwrap();
        assert_eq!(decompressed, record);
    }

    #[test]
    fn compress_rejects_wrong_prefix() {
        let record = vec![0x00, 0x05, b'h', b'e', b'l', b'l', b'o'];
        assert!(compress_record(&record, b"xyz").is_none());
    }

    #[test]
    fn compression_ratio_example() {
        let prefix = b"http://example.org/";
        let keys = [
            b"http://example.org/node/1",
            b"http://example.org/node/2",
            b"http://example.org/node/3",
        ];
        let mut total_raw = 0usize;
        let mut total_compressed = 0usize;
        for key in &keys {
            let mut record = Vec::new();
            record.extend_from_slice(&(key.len() as u16).to_be_bytes());
            record.extend_from_slice(*key);
            total_raw += record.len();
            let compressed = compress_record(&record, prefix.as_slice()).unwrap();
            total_compressed += compressed.len();
        }
        assert!(total_compressed < total_raw, "compression must reduce size");
    }
}
