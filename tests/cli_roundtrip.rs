//! Integration tests for the CLI import/export round-trip (Task 179).
//!
//! These exercise the full durable path: parse a fixture, persist it through
//! the real engine to a temp database, reopen, enumerate the graph, and export
//! it — asserting the exported content reflects what was imported.

use rgraph::cli::{self, Format};
use rgraph::graph::engine::GraphStorageEngine;
use rgraph::graph::graph::Graph;
use rgraph::io::posix::PosixFileSystem;

/// Import `content` in `format` into a fresh engine at `db_path`, returning the
/// import counts.  The engine is dropped (and thus fully synced) before return.
fn import_to_db(
    db_path: &std::path::Path,
    content: &str,
    format: Format,
    fs: &PosixFileSystem,
) -> cli::ImportCounts {
    let engine = GraphStorageEngine::init(db_path.to_path_buf(), fs).unwrap();
    let mut graph = Graph::new(engine);
    let records = cli::import::parse(content, format).unwrap();
    cli::import::apply_records(&mut graph, &records, fs).unwrap()
}

/// Reopen the engine at `db_path` and export it in `format` to a `String`.
fn export_from_db(db_path: &std::path::Path, format: Format, fs: &PosixFileSystem) -> String {
    let engine = GraphStorageEngine::open(db_path.to_path_buf(), fs).unwrap();
    let graph = Graph::new(engine);
    let mut buf: Vec<u8> = Vec::new();
    cli::export::export(&graph, format, fs, &mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn csv_import_then_cypher_export_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);

    // Two Person nodes (one CSV file) and one KNOWS edge (a second CSV file),
    // mirroring the real CLI flow of `import nodes.csv` then `import edges.csv`.
    let nodes_csv = "_id,_label,name,age\na,Person,Alice,30\nb,Person,Bob,25\n";
    let edges_csv = "_src,_dst,_type,since\na,b,KNOWS,2020\n";

    // Run 1: import the node file into a fresh database.
    let node_counts = import_to_db(&db_path, nodes_csv, Format::Csv, &fs);
    assert_eq!(node_counts.nodes, 2, "two Person nodes imported");
    assert_eq!(node_counts.edges, 0);

    // Run 2: reopen the same database and import the edge file.  The edge
    // resolves its endpoints via the persisted `_key` property, even though it
    // is a separate import run with a fresh in-memory key map.
    {
        let engine = GraphStorageEngine::open(db_path.clone(), &fs).unwrap();
        let mut graph = Graph::new(engine);
        let edge_records = cli::import::parse(edges_csv, Format::Csv).unwrap();
        let edge_counts = cli::import::apply_records(&mut graph, &edge_records, &fs).unwrap();
        assert_eq!(edge_counts.edges, 1, "edge resolves endpoints across runs");
    }

    // Export the full graph as Cypher and assert both nodes and the edge appear.
    let cypher = export_from_db(&db_path, Format::Cypher, &fs);
    assert!(cypher.contains(":Person"), "cypher must mention Person label: {cypher}");
    assert!(cypher.contains("name: 'Alice'"), "Alice must be present: {cypher}");
    assert!(cypher.contains("name: 'Bob'"), "Bob must be present: {cypher}");
    assert!(cypher.contains("-[:KNOWS"), "KNOWS edge must be present: {cypher}");
    assert!(cypher.contains("since: 2020"), "edge property must be present: {cypher}");
    // The reserved bookkeeping keys must not leak into the exported properties.
    assert!(!cypher.contains("_key:"), "internal _key must not be exported: {cypher}");
    assert!(!cypher.contains("_label:"), "internal _label must not be exported: {cypher}");
}

#[test]
fn jsonl_import_then_jsonl_export_preserves_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);

    // Nodes and edge in one JSONL document so the edge resolves both endpoints.
    let jsonl = r#"{"type":"node","id":"a","label":"Person","name":"Alice","age":30}
{"type":"node","id":"b","label":"Person","name":"Bob"}
{"type":"edge","src":"a","dst":"b","relType":"KNOWS","since":2020}"#;

    let counts = import_to_db(&db_path, jsonl, Format::Jsonl, &fs);
    assert_eq!(counts.nodes, 2, "two nodes imported");
    assert_eq!(counts.edges, 1, "one edge imported");

    // Reopen and export as JSONL; parse each line back and verify structure.
    let exported = export_from_db(&db_path, Format::Jsonl, &fs);
    let mut node_count = 0;
    let mut edge_count = 0;
    let mut saw_alice = false;
    let mut saw_knows = false;
    for line in exported.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        match v["type"].as_str().unwrap() {
            "node" => {
                node_count += 1;
                assert_eq!(v["label"], "Person");
                if v["props"]["name"] == "Alice" {
                    saw_alice = true;
                    assert_eq!(v["props"]["age"], 30);
                }
            }
            "edge" => {
                edge_count += 1;
                assert_eq!(v["relType"], "KNOWS");
                saw_knows = true;
            }
            other => panic!("unexpected record type {other}"),
        }
    }
    assert_eq!(node_count, 2, "exported two nodes");
    assert_eq!(edge_count, 1, "exported one edge");
    assert!(saw_alice, "Alice node present in export");
    assert!(saw_knows, "KNOWS edge present in export");
}

#[test]
fn turtle_import_then_turtle_export_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);

    let ttl = "<alice> <name> \"Alice\" .\n<alice> <knows> <bob> .\n";
    let counts = import_to_db(&db_path, ttl, Format::Turtle, &fs);
    assert_eq!(counts.nodes, 2, "alice and bob nodes");
    assert_eq!(counts.edges, 1, "alice knows bob");

    let exported = export_from_db(&db_path, Format::Turtle, &fs);
    // Each node has a _label triple, alice has a name triple, and there is one
    // edge triple. The node ids are server-allocated so we assert on structure.
    let label_triples = exported.matches("<_label>").count();
    assert_eq!(label_triples, 2, "two node label triples: {exported}");
    assert!(exported.contains("\"Alice\""), "Alice literal preserved: {exported}");
    assert!(exported.contains("<knows>"), "knows predicate present: {exported}");
}

#[test]
fn benchmark_reports_positive_throughput_on_tiny_workload() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    let engine = GraphStorageEngine::init(db_path, &fs).unwrap();
    let mut graph = Graph::new(engine);

    let report = cli::benchmark::run(&mut graph, 20, 20, &fs).unwrap();
    assert_eq!(report.insert.count, 20);
    assert_eq!(report.read.count, 20);
    assert!(report.insert.ops_per_sec > 0.0);
    assert!(report.read.ops_per_sec > 0.0);

    let table = report.render_table();
    assert!(table.contains("insert"));
    assert!(table.contains("read"));
}
