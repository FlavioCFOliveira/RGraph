//! Export the full graph as Cypher `CREATE` statements, JSONL, or Turtle.
//!
//! The exporter enumerates every live node and relationship through the
//! [`Graph`] public API and serialises them.  Textual labels / types are
//! recovered from the reserved [`LABEL_KEY`] / [`TYPE_KEY`] properties written
//! by the importer, so a round-trip survives an engine restart.

use super::{CliError, Format, KEY_KEY, LABEL_KEY, TYPE_KEY};
use crate::graph::graph::{Graph, Node, Relationship};
use crate::graph::property::{OrderedF64, Property};
use crate::io::FileSystem;
use std::io::Write;

/// Serialise the whole graph in `format`, writing to `out`.
///
/// Returns the number of `(nodes, edges)` written.
///
/// # Errors
///
/// Propagates [`CliError::Storage`] on engine read failure and
/// [`CliError::Io`] on a write failure.
pub fn export<W: Write>(
    graph: &Graph,
    format: Format,
    fs: &dyn FileSystem,
    out: &mut W,
) -> Result<(usize, usize), CliError> {
    let nodes = graph.scan_all_nodes(fs)?;
    let edges = graph.scan_all_relationships(fs)?;

    match format {
        Format::Cypher => write_cypher(&nodes, &edges, out)?,
        Format::Jsonl => write_jsonl(&nodes, &edges, out)?,
        Format::Turtle => write_turtle(&nodes, &edges, out)?,
        Format::Csv => {
            return Err(CliError::UnknownFormat(
                "csv export is not supported; use cypher, jsonl, or turtle".into(),
            ));
        }
    }
    Ok((nodes.len(), edges.len()))
}

/// Recover a node's textual label from its [`LABEL_KEY`] property, falling back
/// to `L{label_id}` when the property is absent.
fn node_label(node: &Node) -> String {
    match node.properties.get(LABEL_KEY) {
        Some(Property::String(s)) => s.clone(),
        _ => format!("L{}", node.label_id),
    }
}

/// Recover an edge's textual type from its [`TYPE_KEY`] property, falling back
/// to `T{type_id}`.
fn edge_type(edge: &Relationship) -> String {
    match edge.properties.get(TYPE_KEY) {
        Some(Property::String(s)) => s.clone(),
        _ => format!("T{}", edge.type_id),
    }
}

/// Iterate a node/edge property map excluding the reserved bookkeeping keys,
/// in deterministic (sorted) order so output is stable across runs.
fn user_properties(
    props: &std::collections::HashMap<String, Property>,
) -> Vec<(&String, &Property)> {
    let mut entries: Vec<(&String, &Property)> = props
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k != LABEL_KEY && k != TYPE_KEY && k != KEY_KEY
        })
        .collect();
    entries.sort_by_key(|(a, _)| *a);
    entries
}

// ----------------------------------------------------------------------------
// Cypher
// ----------------------------------------------------------------------------

fn write_cypher<W: Write>(
    nodes: &[Node],
    edges: &[Relationship],
    out: &mut W,
) -> Result<(), CliError> {
    for node in nodes {
        let label = node_label(node);
        let props = cypher_property_map(&user_properties(&node.properties));
        // `id` is encoded as a deterministic Cypher variable so relationships
        // can MATCH the endpoints; we also pin the logical id as a property.
        writeln!(
            out,
            "CREATE (n{}:{} {{_nid: {}{}}});",
            node.node_id,
            escape_label(&label),
            node.node_id,
            if props.is_empty() {
                String::new()
            } else {
                format!(", {props}")
            }
        )?;
    }
    for edge in edges {
        let rel = edge_type(edge);
        let props = cypher_property_map(&user_properties(&edge.properties));
        let prop_clause = if props.is_empty() {
            String::new()
        } else {
            format!(" {{{props}}}")
        };
        writeln!(
            out,
            "MATCH (a {{_nid: {}}}), (b {{_nid: {}}}) CREATE (a)-[:{}{}]->(b);",
            edge.source_id,
            edge.target_id,
            escape_label(&rel),
            prop_clause
        )?;
    }
    Ok(())
}

/// Render a property list as a Cypher inline-map body (without braces).
fn cypher_property_map(props: &[(&String, &Property)]) -> String {
    props
        .iter()
        .map(|(k, v)| format!("{}: {}", escape_key(k), cypher_value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn cypher_value(p: &Property) -> String {
    match p {
        Property::Null => "null".to_owned(),
        Property::Boolean(b) => b.to_string(),
        Property::Integer(i) => i.to_string(),
        Property::Float(OrderedF64(f)) => format_float(*f),
        Property::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Property::Date(i) | Property::Duration(i) => i.to_string(),
        _ => "null".to_owned(),
    }
}

/// Format a property key as a Cypher identifier, quoting with backticks when it
/// is not a bare identifier.
fn escape_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        k.to_owned()
    } else {
        format!("`{}`", k.replace('`', "``"))
    }
}

/// Format a label/type as a Cypher identifier, backtick-quoting if needed.
fn escape_label(l: &str) -> String {
    escape_key(l)
}

/// Render an `f64` without losing integral precision (`3.0` not `3`).
fn format_float(f: f64) -> String {
    if f == f.trunc() && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

// ----------------------------------------------------------------------------
// JSONL
// ----------------------------------------------------------------------------

fn write_jsonl<W: Write>(
    nodes: &[Node],
    edges: &[Relationship],
    out: &mut W,
) -> Result<(), CliError> {
    for node in nodes {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), serde_json::Value::String("node".into()));
        map.insert("id".into(), serde_json::Value::from(node.node_id));
        map.insert("label".into(), serde_json::Value::String(node_label(node)));
        map.insert("props".into(), property_json(&user_properties(&node.properties)));
        writeln!(out, "{}", serde_json::Value::Object(map))?;
    }
    for edge in edges {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), serde_json::Value::String("edge".into()));
        map.insert("id".into(), serde_json::Value::from(edge.edge_id));
        map.insert("src".into(), serde_json::Value::from(edge.source_id));
        map.insert("dst".into(), serde_json::Value::from(edge.target_id));
        map.insert("relType".into(), serde_json::Value::String(edge_type(edge)));
        map.insert("props".into(), property_json(&user_properties(&edge.properties)));
        writeln!(out, "{}", serde_json::Value::Object(map))?;
    }
    Ok(())
}

fn property_json(props: &[(&String, &Property)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in props {
        if let Some(jv) = property_to_json(v) {
            map.insert((*k).clone(), jv);
        }
    }
    serde_json::Value::Object(map)
}

fn property_to_json(p: &Property) -> Option<serde_json::Value> {
    match p {
        Property::Null => Some(serde_json::Value::Null),
        Property::Boolean(b) => Some(serde_json::Value::Bool(*b)),
        Property::Integer(i) => Some(serde_json::Value::from(*i)),
        Property::Float(OrderedF64(f)) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number),
        Property::String(s) => Some(serde_json::Value::String(s.clone())),
        Property::Date(i) | Property::Duration(i) => Some(serde_json::Value::from(*i)),
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// Turtle
// ----------------------------------------------------------------------------

fn write_turtle<W: Write>(
    nodes: &[Node],
    edges: &[Relationship],
    out: &mut W,
) -> Result<(), CliError> {
    // Each node id becomes an IRI term `<n{id}>`.  Literal properties become
    // `<n{id}> <key> "value" .` statements; edges become IRI-object triples.
    for node in nodes {
        let subj = format!("<n{}>", node.node_id);
        writeln!(out, "{} <_label> \"{}\" .", subj, turtle_escape(&node_label(node)))?;
        for (k, v) in user_properties(&node.properties) {
            writeln!(out, "{} <{}> {} .", subj, k, turtle_value(v))?;
        }
    }
    for edge in edges {
        writeln!(
            out,
            "<n{}> <{}> <n{}> .",
            edge.source_id,
            edge_type(edge),
            edge.target_id
        )?;
    }
    Ok(())
}

fn turtle_value(p: &Property) -> String {
    match p {
        Property::Integer(i) => i.to_string(),
        Property::Float(OrderedF64(f)) => format_float(*f),
        Property::Boolean(b) => format!("\"{b}\""),
        Property::String(s) => format!("\"{}\"", turtle_escape(s)),
        _ => "\"\"".to_owned(),
    }
}

fn turtle_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(id: u64, label: &str, name: &str) -> Node {
        let mut props = HashMap::new();
        props.insert(LABEL_KEY.to_owned(), Property::String(label.to_owned()));
        props.insert("name".to_owned(), Property::String(name.to_owned()));
        Node {
            node_id: id,
            label_id: 1,
            properties: props,
        }
    }

    fn edge(id: u64, src: u64, dst: u64, ty: &str) -> Relationship {
        let mut props = HashMap::new();
        props.insert(TYPE_KEY.to_owned(), Property::String(ty.to_owned()));
        Relationship {
            edge_id: id,
            type_id: 1,
            source_id: src,
            target_id: dst,
            properties: props,
        }
    }

    #[test]
    fn cypher_output_contains_create_and_match() {
        let nodes = vec![node(1, "Person", "Alice"), node(2, "Person", "Bob")];
        let edges = vec![edge(1, 1, 2, "KNOWS")];
        let mut buf = Vec::new();
        write_cypher(&nodes, &edges, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("CREATE (n1:Person"), "got: {s}");
        assert!(s.contains("name: 'Alice'"), "got: {s}");
        assert!(s.contains("-[:KNOWS]->"), "got: {s}");
        assert!(s.contains("_nid: 1"), "got: {s}");
    }

    #[test]
    fn jsonl_output_roundtrips_via_serde() {
        let nodes = vec![node(1, "Person", "Alice")];
        let edges = vec![];
        let mut buf = Vec::new();
        write_jsonl(&nodes, &edges, &mut buf).unwrap();
        let line = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "node");
        assert_eq!(v["label"], "Person");
        assert_eq!(v["props"]["name"], "Alice");
    }

    #[test]
    fn turtle_output_has_triples() {
        let nodes = vec![node(1, "Person", "Alice")];
        let edges = vec![edge(1, 1, 2, "KNOWS")];
        let mut buf = Vec::new();
        write_turtle(&nodes, &edges, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("<n1> <_label> \"Person\" ."), "got: {s}");
        assert!(s.contains("<n1> <KNOWS> <n2> ."), "got: {s}");
    }

    #[test]
    fn escape_key_backticks_non_identifier() {
        assert_eq!(escape_key("name"), "name");
        assert_eq!(escape_key("first name"), "`first name`");
    }
}
