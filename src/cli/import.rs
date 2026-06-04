//! Bulk import of nodes and edges into the graph engine.
//!
//! Parsing is separated from persistence: each format is turned into a vector
//! of [`ImportRecord`]s, which [`apply_records`] then materialises through the
//! [`Graph`] public API.  This keeps the parsers unit-testable without touching
//! disk and guarantees every code path drives the real engine.

use super::{CliError, Format, ImportCounts, KEY_KEY, LABEL_KEY, TYPE_KEY};
use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
use crate::graph::graph::Graph;
use crate::graph::property::Property;
use crate::io::FileSystem;
use std::collections::HashMap;

/// A single parsed import record: either a node or an edge.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportRecord {
    /// A node.  `key` is a caller-supplied logical identifier used to wire up
    /// edges within the same import; it is *not* the server-allocated id.
    Node {
        /// Caller-supplied logical key (e.g. CSV `_id`), if present.
        key: Option<String>,
        /// Textual label (stored as the [`LABEL_KEY`] property).
        label: String,
        /// User properties (excluding the reserved keys).
        properties: HashMap<String, Property>,
    },
    /// An edge between two previously declared nodes, referenced by their
    /// logical keys.
    Edge {
        /// Source node logical key.
        src: String,
        /// Target node logical key.
        dst: String,
        /// Textual relationship type (stored as the [`TYPE_KEY`] property).
        rel_type: String,
        /// User properties (excluding the reserved keys).
        properties: HashMap<String, Property>,
    },
}

/// Parse `content` in the given `format` into a list of [`ImportRecord`]s.
///
/// # Errors
///
/// Returns [`CliError::Parse`] on malformed input.
pub fn parse(content: &str, format: Format) -> Result<Vec<ImportRecord>, CliError> {
    match format {
        Format::Csv => parse_csv(content),
        Format::Jsonl => parse_jsonl(content),
        Format::Turtle => parse_turtle(content),
        Format::Cypher => Err(CliError::UnknownFormat(
            "cypher import is not supported; use csv, jsonl, or turtle".into(),
        )),
    }
}

/// Apply parsed records to `graph`, allocating server ids and wiring edges by
/// their logical keys.  Each record is created through the engine API; the
/// engine is synced once at the end so the import is durable.
///
/// Edges whose endpoints were never declared as nodes are skipped (counted as
/// neither created nor an error) — a deliberately lenient policy so a partial
/// edge file does not abort the whole load.
///
/// # Errors
///
/// Propagates [`CliError::Storage`] if the engine rejects a write.
pub fn apply_records(
    graph: &mut Graph,
    records: &[ImportRecord],
    fs: &dyn FileSystem,
) -> Result<ImportCounts, CliError> {
    let mut counts = ImportCounts::default();
    // Map from the caller's logical key to the server-allocated node id.
    // Seed it from nodes already on disk so that edges imported in a *separate*
    // run (or file) can still resolve their endpoints by logical key.
    let mut key_to_id: HashMap<String, u64> = seed_key_map(graph, fs)?;
    // Auto-generated keys for nodes without an explicit key, so edges can still
    // reference the most recently created node if needed.
    let mut anon_counter: u64 = 0;

    for record in records {
        match record {
            ImportRecord::Node {
                key,
                label,
                properties,
            } => {
                let label_id = graph.engine_mut().catalog_label_id(label);
                let mut builder = NodeBuilder::new().label(label_id);
                builder = builder.property(LABEL_KEY, label.as_str());
                let logical_key = key.clone().unwrap_or_else(|| {
                    anon_counter += 1;
                    format!("__anon_{anon_counter}")
                });
                // Persist the logical key so cross-run edge resolution works.
                builder = builder.property(KEY_KEY, logical_key.as_str());
                for (k, v) in properties {
                    builder = builder.property(k.clone(), v.clone());
                }
                let (_slot, node_id) = graph.create_node(builder, fs)?;
                counts.nodes += 1;

                key_to_id.insert(logical_key, node_id);
            }
            ImportRecord::Edge {
                src,
                dst,
                rel_type,
                properties,
            } => {
                let (Some(&src_id), Some(&dst_id)) =
                    (key_to_id.get(src), key_to_id.get(dst))
                else {
                    // Endpoint not found: skip leniently.
                    continue;
                };
                let type_id = graph.engine_mut().catalog_label_id(rel_type);
                let mut builder = RelationshipBuilder::new()
                    .from(src_id)
                    .to(dst_id)
                    .type_id(type_id);
                builder = builder.property(TYPE_KEY, rel_type.as_str());
                for (k, v) in properties {
                    builder = builder.property(k.clone(), v.clone());
                }
                graph.create_relationship(builder, fs)?;
                counts.edges += 1;
            }
        }
    }

    graph.sync(fs)?;
    Ok(counts)
}

/// Build the initial logical-key → server-id map from nodes already persisted
/// in `graph`, reading each node's [`KEY_KEY`] property.  This lets an edge
/// import resolve endpoints that were created in an earlier import run.
fn seed_key_map(
    graph: &Graph,
    fs: &dyn FileSystem,
) -> Result<HashMap<String, u64>, CliError> {
    let mut map = HashMap::new();
    for node in graph.scan_all_nodes(fs)? {
        if let Some(Property::String(key)) = node.properties.get(KEY_KEY) {
            map.insert(key.clone(), node.node_id);
        }
    }
    Ok(map)
}

// ----------------------------------------------------------------------------
// CSV
// ----------------------------------------------------------------------------

/// Parse a header-driven CSV.
///
/// Column semantics (case-insensitive header match):
/// * `_id`   — node logical key.
/// * `_label`— node label.
/// * `_src`, `_dst` — edge endpoints (their presence marks the file as edges).
/// * `_type` — edge relationship type.
/// * any other column — a property; values are typed by [`infer_value`].
fn parse_csv(content: &str) -> Result<Vec<ImportRecord>, CliError> {
    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    let header_line = match lines.next() {
        Some(h) => h,
        None => return Ok(Vec::new()),
    };
    let header: Vec<String> = split_csv(header_line)
        .into_iter()
        .map(|s| s.trim().to_owned())
        .collect();

    let lower: Vec<String> = header.iter().map(|h| h.to_ascii_lowercase()).collect();
    let is_edge = lower.iter().any(|h| h == "_src") && lower.iter().any(|h| h == "_dst");

    let col = |name: &str| -> Option<usize> { lower.iter().position(|h| h == name) };

    let mut records = Vec::new();
    for (line_no, line) in lines.enumerate() {
        let fields = split_csv(line);
        if fields.len() != header.len() {
            return Err(CliError::Parse(format!(
                "CSV row {} has {} fields but header has {}",
                line_no + 2,
                fields.len(),
                header.len()
            )));
        }

        let mut properties = HashMap::new();
        for (i, raw) in fields.iter().enumerate() {
            let name = &lower[i];
            if name.starts_with('_') {
                continue; // reserved columns handled below
            }
            let v = raw.trim();
            if v.is_empty() {
                continue;
            }
            properties.insert(header[i].clone(), infer_value(v));
        }

        if is_edge {
            let src = col("_src").map(|i| fields[i].trim().to_owned()).unwrap_or_default();
            let dst = col("_dst").map(|i| fields[i].trim().to_owned()).unwrap_or_default();
            let rel_type = col("_type")
                .map(|i| fields[i].trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "RELATED".to_owned());
            records.push(ImportRecord::Edge {
                src,
                dst,
                rel_type,
                properties,
            });
        } else {
            let key = col("_id")
                .map(|i| fields[i].trim().to_owned())
                .filter(|s| !s.is_empty());
            let label = col("_label")
                .map(|i| fields[i].trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Node".to_owned());
            records.push(ImportRecord::Node {
                key,
                label,
                properties,
            });
        }
    }
    Ok(records)
}

/// Split one CSV line on commas, honouring double-quoted fields (with `""`
/// escaping a literal quote).  Sufficient for the import surface; not a full
/// RFC-4180 parser (no embedded newlines).
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut field));
            }
            _ => field.push(c),
        }
    }
    out.push(field);
    out
}

/// Infer a [`Property`] value from a raw textual field: integer, float, bool,
/// or fallback to string.
fn infer_value(raw: &str) -> Property {
    if let Ok(i) = raw.parse::<i64>() {
        return Property::Integer(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Property::from(f);
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return Property::Boolean(true),
        "false" => return Property::Boolean(false),
        _ => {}
    }
    Property::String(raw.to_owned())
}

// ----------------------------------------------------------------------------
// JSONL
// ----------------------------------------------------------------------------

/// Parse JSON Lines.  Each non-empty line must be a JSON object with a `type`
/// field of `"node"` or `"edge"`.
///
/// Node object: `{"type":"node","id":"a","label":"Person","props":{…}}`
/// (or flat top-level properties besides the reserved keys).
/// Edge object: `{"type":"edge","src":"a","dst":"b","relType":"KNOWS","props":{…}}`.
fn parse_jsonl(content: &str) -> Result<Vec<ImportRecord>, CliError> {
    let mut records = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| CliError::Parse(format!("JSONL line {}: {e}", line_no + 1)))?;
        let obj = value.as_object().ok_or_else(|| {
            CliError::Parse(format!("JSONL line {} is not an object", line_no + 1))
        })?;

        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("node")
            .to_ascii_lowercase();

        // Collect explicit `props` plus flat top-level keys (minus reserved).
        let mut properties = HashMap::new();
        if let Some(props) = obj.get("props").and_then(|v| v.as_object()) {
            for (k, v) in props {
                if let Some(p) = json_to_property(v) {
                    properties.insert(k.clone(), p);
                }
            }
        }

        match kind.as_str() {
            "node" => {
                let key = obj.get("id").and_then(json_string_field);
                let label = obj
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Node")
                    .to_owned();
                merge_flat_properties(
                    obj,
                    &["type", "id", "label", "props"],
                    &mut properties,
                );
                records.push(ImportRecord::Node {
                    key,
                    label,
                    properties,
                });
            }
            "edge" => {
                let src = obj
                    .get("src")
                    .and_then(json_string_field)
                    .ok_or_else(|| {
                        CliError::Parse(format!("JSONL line {}: edge missing src", line_no + 1))
                    })?;
                let dst = obj
                    .get("dst")
                    .and_then(json_string_field)
                    .ok_or_else(|| {
                        CliError::Parse(format!("JSONL line {}: edge missing dst", line_no + 1))
                    })?;
                let rel_type = obj
                    .get("relType")
                    .or_else(|| obj.get("type_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("RELATED")
                    .to_owned();
                merge_flat_properties(
                    obj,
                    &["type", "src", "dst", "relType", "type_name", "props"],
                    &mut properties,
                );
                records.push(ImportRecord::Edge {
                    src,
                    dst,
                    rel_type,
                    properties,
                });
            }
            other => {
                return Err(CliError::Parse(format!(
                    "JSONL line {}: unknown record type '{other}'",
                    line_no + 1
                )));
            }
        }
    }
    Ok(records)
}

/// Read a JSON field as a string, coercing numbers to their textual form so
/// numeric ids can be used as logical keys.
fn json_string_field(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Fold flat top-level object entries (excluding `exclude` keys) into `out`.
fn merge_flat_properties(
    obj: &serde_json::Map<String, serde_json::Value>,
    exclude: &[&str],
    out: &mut HashMap<String, Property>,
) {
    for (k, v) in obj {
        if exclude.contains(&k.as_str()) {
            continue;
        }
        if out.contains_key(k) {
            continue;
        }
        if let Some(p) = json_to_property(v) {
            out.insert(k.clone(), p);
        }
    }
}

/// Convert a scalar JSON value into a [`Property`].  Composite values
/// (arrays/objects) are skipped because the on-disk codec does not yet support
/// them.
fn json_to_property(v: &serde_json::Value) -> Option<Property> {
    match v {
        serde_json::Value::Bool(b) => Some(Property::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Property::Integer(i))
            } else {
                n.as_f64().map(Property::from)
            }
        }
        serde_json::Value::String(s) => Some(Property::String(s.clone())),
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// Turtle (basic)
// ----------------------------------------------------------------------------

/// Parse a minimal subset of RDF Turtle: one `subject predicate object .`
/// triple per statement.  Subjects and objects become nodes (label `Resource`)
/// keyed by their term text; predicates become relationships.
///
/// Literal objects (quoted strings or bare numbers) are attached to the subject
/// node as a property named after the predicate instead of creating an edge.
/// This is deliberately small but real — enough to round-trip the Turtle we
/// emit on export.
fn parse_turtle(content: &str) -> Result<Vec<ImportRecord>, CliError> {
    let mut node_keys: HashMap<String, ()> = HashMap::new();
    let mut nodes: Vec<ImportRecord> = Vec::new();
    let mut edges: Vec<ImportRecord> = Vec::new();
    // Accumulate literal properties per subject before emitting node records.
    let mut subject_props: HashMap<String, HashMap<String, Property>> = HashMap::new();
    let mut subject_order: Vec<String> = Vec::new();

    for (line_no, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("@prefix") {
            continue;
        }
        let stmt = line.trim_end_matches('.').trim();
        if stmt.is_empty() {
            continue;
        }
        let terms = tokenize_turtle(stmt);
        if terms.len() < 3 {
            return Err(CliError::Parse(format!(
                "Turtle line {}: expected 'subject predicate object', got '{stmt}'",
                line_no + 1
            )));
        }
        let subject = clean_iri(&terms[0]);
        let predicate = clean_iri(&terms[1]);
        let object = terms[2].clone();

        if !subject_props.contains_key(&subject) {
            subject_props.insert(subject.clone(), HashMap::new());
            subject_order.push(subject.clone());
        }

        if let Some(literal) = parse_turtle_literal(&object) {
            // Literal: attach as a property of the subject node.
            subject_props
                .get_mut(&subject)
                .expect("subject inserted above")
                .insert(predicate, literal);
        } else {
            // IRI object: create the target node and an edge.
            let target = clean_iri(&object);
            if node_keys.insert(target.clone(), ()).is_none() {
                nodes.push(ImportRecord::Node {
                    key: Some(target.clone()),
                    label: "Resource".to_owned(),
                    properties: HashMap::new(),
                });
            }
            edges.push(ImportRecord::Edge {
                src: subject.clone(),
                dst: target,
                rel_type: predicate,
                properties: HashMap::new(),
            });
        }
    }

    // Emit subject nodes (with accumulated literal properties) first, in the
    // order they were first seen, then objects, then edges — so every edge
    // endpoint exists by the time edges are applied.
    let mut ordered_nodes = Vec::new();
    for subj in &subject_order {
        node_keys.insert(subj.clone(), ());
        ordered_nodes.push(ImportRecord::Node {
            key: Some(subj.clone()),
            label: "Resource".to_owned(),
            properties: subject_props.remove(subj).unwrap_or_default(),
        });
    }
    // Object-only nodes that were not also subjects.
    for n in nodes {
        if let ImportRecord::Node { key: Some(k), .. } = &n
            && !subject_order.contains(k)
        {
            ordered_nodes.push(n);
        }
    }

    ordered_nodes.extend(edges);
    Ok(ordered_nodes)
}

/// Split a Turtle statement into whitespace-delimited terms, keeping quoted
/// literals (which may contain spaces) intact.
fn tokenize_turtle(stmt: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in stmt.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    terms.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    terms
}

/// Strip `<...>` IRI delimiters and prefix syntax, returning a bare key.
fn clean_iri(term: &str) -> String {
    term.trim_start_matches('<')
        .trim_end_matches('>')
        .to_owned()
}

/// Parse a Turtle object term as a literal property, or `None` if it is an IRI
/// (and should therefore become an edge).
fn parse_turtle_literal(object: &str) -> Option<Property> {
    let trimmed = object.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        // Quoted string literal, possibly with a datatype suffix we ignore.
        let end = rest.find('"')?;
        return Some(Property::String(rest[..end].to_owned()));
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(Property::Integer(i));
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        return Some(Property::from(f));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_csv_nodes() {
        let csv = "_id,_label,name,age\na,Person,Alice,30\nb,Person,Bob,25\n";
        let recs = parse_csv(csv).unwrap();
        assert_eq!(recs.len(), 2);
        match &recs[0] {
            ImportRecord::Node {
                key,
                label,
                properties,
            } => {
                assert_eq!(key.as_deref(), Some("a"));
                assert_eq!(label, "Person");
                assert_eq!(properties.get("name"), Some(&Property::String("Alice".into())));
                assert_eq!(properties.get("age"), Some(&Property::Integer(30)));
            }
            other => panic!("expected node, got {other:?}"),
        }
    }

    #[test]
    fn parse_csv_edges_detected_by_src_dst() {
        let csv = "_src,_dst,_type,since\na,b,KNOWS,2020\n";
        let recs = parse_csv(csv).unwrap();
        assert_eq!(recs.len(), 1);
        match &recs[0] {
            ImportRecord::Edge {
                src,
                dst,
                rel_type,
                properties,
            } => {
                assert_eq!(src, "a");
                assert_eq!(dst, "b");
                assert_eq!(rel_type, "KNOWS");
                assert_eq!(properties.get("since"), Some(&Property::Integer(2020)));
            }
            other => panic!("expected edge, got {other:?}"),
        }
    }

    #[test]
    fn parse_csv_quoted_field_with_comma() {
        let csv = "_id,_label,name\na,Person,\"Doe, John\"\n";
        let recs = parse_csv(csv).unwrap();
        match &recs[0] {
            ImportRecord::Node { properties, .. } => {
                assert_eq!(
                    properties.get("name"),
                    Some(&Property::String("Doe, John".into()))
                );
            }
            other => panic!("expected node, got {other:?}"),
        }
    }

    #[test]
    fn parse_csv_rejects_ragged_row() {
        let csv = "_id,_label,name\na,Person\n";
        assert!(matches!(parse_csv(csv), Err(CliError::Parse(_))));
    }

    #[test]
    fn parse_jsonl_nodes_and_edges() {
        let jsonl = r#"{"type":"node","id":"a","label":"Person","props":{"name":"Alice"}}
{"type":"node","id":"b","label":"Person","name":"Bob","age":25}
{"type":"edge","src":"a","dst":"b","relType":"KNOWS","props":{"since":2020}}"#;
        let recs = parse_jsonl(jsonl).unwrap();
        assert_eq!(recs.len(), 3);
        match &recs[1] {
            ImportRecord::Node {
                key,
                label,
                properties,
            } => {
                assert_eq!(key.as_deref(), Some("b"));
                assert_eq!(label, "Person");
                assert_eq!(properties.get("name"), Some(&Property::String("Bob".into())));
                assert_eq!(properties.get("age"), Some(&Property::Integer(25)));
            }
            other => panic!("expected node, got {other:?}"),
        }
        match &recs[2] {
            ImportRecord::Edge { src, dst, rel_type, .. } => {
                assert_eq!(src, "a");
                assert_eq!(dst, "b");
                assert_eq!(rel_type, "KNOWS");
            }
            other => panic!("expected edge, got {other:?}"),
        }
    }

    #[test]
    fn parse_jsonl_numeric_id_coerced_to_string() {
        let jsonl = r#"{"type":"node","id":1,"label":"N"}"#;
        let recs = parse_jsonl(jsonl).unwrap();
        match &recs[0] {
            ImportRecord::Node { key, .. } => assert_eq!(key.as_deref(), Some("1")),
            other => panic!("expected node, got {other:?}"),
        }
    }

    #[test]
    fn parse_turtle_iri_and_literal() {
        let ttl = "<alice> <name> \"Alice\" .\n<alice> <knows> <bob> .\n";
        let recs = parse_turtle(ttl).unwrap();
        // alice node (with name property), bob node, and one edge.
        let node_count = recs
            .iter()
            .filter(|r| matches!(r, ImportRecord::Node { .. }))
            .count();
        let edge_count = recs
            .iter()
            .filter(|r| matches!(r, ImportRecord::Edge { .. }))
            .count();
        assert_eq!(node_count, 2, "alice and bob");
        assert_eq!(edge_count, 1, "alice -knows-> bob");

        let alice = recs
            .iter()
            .find_map(|r| match r {
                ImportRecord::Node { key, properties, .. } if key.as_deref() == Some("alice") => {
                    Some(properties)
                }
                _ => None,
            })
            .expect("alice node present");
        assert_eq!(alice.get("name"), Some(&Property::String("Alice".into())));
    }

    #[test]
    fn infer_value_types() {
        assert_eq!(infer_value("42"), Property::Integer(42));
        assert_eq!(infer_value("true"), Property::Boolean(true));
        assert_eq!(infer_value("hello"), Property::String("hello".into()));
        match infer_value("3.5") {
            Property::Float(f) => assert!((f.0 - 3.5).abs() < 1e-9),
            other => panic!("expected float, got {other:?}"),
        }
    }
}
