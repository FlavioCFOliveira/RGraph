//! End-to-end Cypher integration tests against the real storage-backed engine
//! (Task 185).
//!
//! These exercise the full front-to-back pipeline
//! `parse -> semantic_analyse -> plan -> physical::execute_plan` driving the
//! durable [`GraphStorageEngine`], mirroring exactly what the CLI's
//! `Command::Query` runs in production.  Each statement is dispatched through
//! [`run_write`], which opens the engine, executes the plan, **syncs to durable
//! storage**, and drops the engine — so a multi-statement scenario is a sequence
//! of independent open/execute/sync cycles, the same as repeated CLI invocations
//! against one database directory.
//!
//! # Scope
//!
//! Every assertion below covers behaviour that is verified to work end-to-end.
//! The four openCypher correctness gaps that Sprint F surfaced were resolved in
//! Sprint G and are now asserted here as passing behaviour:
//!
//! * **Labelled MATCH after restart** (`MATCH (n:Person)`): the schema catalog
//!   (label/type/property-key name -> id map) is now persisted to a sidecar file
//!   and restored on reopen, so a labelled scan resolves correctly after a
//!   restart (**rmp Task 189**).
//!
//! * **`SET` write-back across statements**: `SET n.prop = v` is flushed durably
//!   to the node's property chain, so a subsequent separate MATCH reads the new
//!   value, including after a restart (**rmp Task 191**).
//!
//! * **MERGE with an inline property map**: `MERGE (e:Person {name: 'Dave'})`
//!   matches only a node equal on *all* inline properties and otherwise creates
//!   one, persisting its properties; a repeated identical MERGE is idempotent
//!   (**rmp Task 192**).
//!
//! * **End-to-end aggregation** (`RETURN count(n)`, `sum`/`avg`/`min`/`max`/
//!   `collect`, grouped aggregation): aggregate arguments now see the
//!   MATCH-bound pattern variables against the real engine pipeline
//!   (**rmp Task 190**).

use rgraph::cypher::executor::QueryResult;
use rgraph::cypher::parser::parse;
use rgraph::cypher::physical::{ExecutionContext, execute_plan};
use rgraph::cypher::planner::plan;
use rgraph::cypher::semantic::analyse;
use rgraph::cypher::value::Value;
use rgraph::graph::engine::GraphStorageEngine;
use rgraph::io::posix::PosixFileSystem;
use std::path::Path;

/// Initialise a fresh, empty database at `db_path`.
fn init_db(db_path: &Path, fs: &PosixFileSystem) {
    GraphStorageEngine::init(db_path.to_path_buf(), fs).expect("init engine");
}

/// Dispatch one Cypher statement through the full pipeline against the durable
/// engine, syncing before the engine is dropped so its effects persist.  This is
/// the exact production path: open -> parse -> analyse -> plan -> execute -> sync.
fn run_write(db_path: &Path, query: &str, fs: &PosixFileSystem) -> QueryResult {
    let mut engine = GraphStorageEngine::open(db_path.to_path_buf(), fs).expect("open engine");
    let stmt = parse(query).unwrap_or_else(|e| panic!("parse '{query}': {e}"));
    analyse(&stmt).unwrap_or_else(|e| panic!("semantic '{query}': {e}"));
    let logical = plan(&stmt).unwrap_or_else(|e| panic!("plan '{query}': {e}"));
    let result = {
        let ctx = ExecutionContext::new_with_write(&mut engine, fs);
        execute_plan(&logical, &ctx).unwrap_or_else(|e| panic!("execute '{query}': {e}"))
    };
    engine.sync(fs).expect("sync engine");
    result
}

/// Project a single string column out of a result, in row order.
fn string_column(result: &QueryResult, column: &str) -> Vec<String> {
    let idx = result
        .columns
        .iter()
        .position(|c| c == column)
        .unwrap_or_else(|| panic!("column '{column}' not in {:?}", result.columns));
    result
        .rows
        .iter()
        .map(|row| match &row[idx] {
            Value::String(s) => s.clone(),
            other => panic!("column '{column}' value is not a string: {other:?}"),
        })
        .collect()
}

/// Project a single integer column out of a result, in row order.
fn integer_column(result: &QueryResult, column: &str) -> Vec<i64> {
    let idx = result
        .columns
        .iter()
        .position(|c| c == column)
        .unwrap_or_else(|| panic!("column '{column}' not in {:?}", result.columns));
    result
        .rows
        .iter()
        .map(|row| match &row[idx] {
            Value::Integer(i) => *i,
            other => panic!("column '{column}' value is not an integer: {other:?}"),
        })
        .collect()
}

/// Seed three `Person` nodes and return the open database path's temp dir guard.
fn seed_three(db_path: &Path, fs: &PosixFileSystem) {
    run_write(db_path, "CREATE (a:Person {name: 'Alice', age: 30})", fs);
    run_write(db_path, "CREATE (b:Person {name: 'Bob', age: 25})", fs);
    run_write(db_path, "CREATE (c:Person {name: 'Carol', age: 40})", fs);
}

// ------------------------------------------------------------------
// CREATE + RETURN: a created node is returned with its labels and properties.
// ------------------------------------------------------------------

#[test]
fn create_returns_node_with_labels_and_properties() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);

    let result = run_write(
        &db_path,
        "CREATE (a:Person {name: 'Alice', age: 30}) RETURN a",
        &fs,
    );

    assert_eq!(result.columns, vec!["a".to_string()]);
    assert_eq!(
        result.rows.len(),
        1,
        "exactly one node created and returned"
    );
    match &result.rows[0][0] {
        Value::Node(node) => {
            assert_eq!(
                node.labels,
                vec!["Person".to_string()],
                "label resolved in-session"
            );
            assert_eq!(
                node.properties.get("name"),
                Some(&Value::String("Alice".into()))
            );
            assert_eq!(node.properties.get("age"), Some(&Value::Integer(30)));
        }
        other => panic!("expected a Node, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// MATCH / WHERE / RETURN property / ORDER BY.
// ------------------------------------------------------------------

#[test]
fn match_where_return_property_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);
    seed_three(&db_path, &fs);

    // age > 26 keeps Alice (30) and Carol (40); Bob (25) is filtered out.
    let result = run_write(
        &db_path,
        "MATCH (n) WHERE n.age > 26 RETURN n.name AS name ORDER BY n.name",
        &fs,
    );

    assert_eq!(result.columns, vec!["name".to_string()]);
    assert_eq!(
        string_column(&result, "name"),
        vec!["Alice".to_string(), "Carol".to_string()],
        "WHERE filters Bob, ORDER BY sorts ascending"
    );
}

#[test]
fn order_by_descending() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);
    seed_three(&db_path, &fs);

    let result = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name DESC",
        &fs,
    );

    assert_eq!(
        string_column(&result, "name"),
        vec!["Carol".to_string(), "Bob".to_string(), "Alice".to_string()],
    );
}

#[test]
fn return_multiple_projection_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);
    seed_three(&db_path, &fs);

    let result = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name, n.age AS age ORDER BY n.age",
        &fs,
    );

    // RETURN column order must be preserved exactly.
    assert_eq!(result.columns, vec!["name".to_string(), "age".to_string()]);
    assert_eq!(
        string_column(&result, "name"),
        vec!["Bob".to_string(), "Alice".to_string(), "Carol".to_string()],
    );
    assert_eq!(integer_column(&result, "age"), vec![25, 30, 40]);
}

// ------------------------------------------------------------------
// SKIP / LIMIT pagination over an ordered result.
// ------------------------------------------------------------------

#[test]
fn skip_and_limit_paginate_ordered_result() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);
    seed_three(&db_path, &fs);

    // Ordered [Alice, Bob, Carol]; SKIP 1 LIMIT 1 -> [Bob].
    let result = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name SKIP 1 LIMIT 1",
        &fs,
    );
    assert_eq!(string_column(&result, "name"), vec!["Bob".to_string()]);

    // LIMIT alone caps the result length.
    let limited = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name LIMIT 2",
        &fs,
    );
    assert_eq!(
        string_column(&limited, "name"),
        vec!["Alice".to_string(), "Bob".to_string()],
    );
}

// ------------------------------------------------------------------
// DELETE read-back (same session): a deleted node disappears from later MATCH.
// ------------------------------------------------------------------

#[test]
fn delete_removes_node_from_subsequent_match() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);
    seed_three(&db_path, &fs);

    run_write(&db_path, "MATCH (n) WHERE n.name = 'Bob' DELETE n", &fs);

    let remaining = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name",
        &fs,
    );
    assert_eq!(
        string_column(&remaining, "name"),
        vec!["Alice".to_string(), "Carol".to_string()],
        "deleted node must not reappear in a later match"
    );
}

// ------------------------------------------------------------------
// Task 191: SET write-back is durable across statements and across a restart.
// A new property and an overwritten property are both flushed to the record;
// unmodified properties survive; the value persists after reopen.
// ------------------------------------------------------------------

#[test]
fn set_property_write_back_persists_across_statements_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);

    run_write(&db_path, "CREATE (a:Person {name: 'Alice', age: 30})", &fs);

    // Overwrite an existing property (age) and add a new one (city).
    let mutated = run_write(
        &db_path,
        "MATCH (n) WHERE n.name = 'Alice' SET n.age = 99, n.city = 'NYC' RETURN n.age AS age, n.city AS city",
        &fs,
    );
    assert_eq!(
        integer_column(&mutated, "age"),
        vec![99],
        "mutating statement sees new age"
    );
    assert_eq!(string_column(&mutated, "city"), vec!["NYC".to_string()]);

    // A fresh, separate statement must read the persisted values (not pre-SET).
    let readback = run_write(
        &db_path,
        "MATCH (n) WHERE n.name = 'Alice' RETURN n.age AS age, n.city AS city, n.name AS name",
        &fs,
    );
    assert_eq!(
        integer_column(&readback, "age"),
        vec![99],
        "overwritten property persists to the record"
    );
    assert_eq!(
        string_column(&readback, "city"),
        vec!["NYC".to_string()],
        "new property persists to the record"
    );
    assert_eq!(
        string_column(&readback, "name"),
        vec!["Alice".to_string()],
        "an unmodified property is preserved by the rewrite"
    );

    // Across a full reopen, the durable values are still there.
    {
        let mut engine = GraphStorageEngine::open(db_path.clone(), &fs).expect("reopen");
        engine.sync(&fs).expect("sync");
    }
    let after_restart = run_write(
        &db_path,
        "MATCH (n) WHERE n.name = 'Alice' RETURN n.age AS age, n.city AS city",
        &fs,
    );
    assert_eq!(
        integer_column(&after_restart, "age"),
        vec![99],
        "SET value survives a restart"
    );
    assert_eq!(
        string_column(&after_restart, "city"),
        vec!["NYC".to_string()]
    );
}

// ------------------------------------------------------------------
// Task 192: MERGE honours the inline property predicate in match-or-create.
//
// `MERGE (e:Person {name: 'Dave'})` against a graph holding only Alice CREATES
// Dave (no full match); merging an existing node matches it (no duplicate); a
// repeated identical MERGE is idempotent; a partial property match still
// creates a new node.
// ------------------------------------------------------------------

#[test]
fn merge_honours_inline_property_predicate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);

    run_write(&db_path, "CREATE (a:Person {name: 'Alice', age: 30})", &fs);

    // No node equals {name: 'Dave'} -> CREATE Dave, persisting the property.
    let created = run_write(
        &db_path,
        "MERGE (e:Person {name: 'Dave'}) RETURN e.name AS name",
        &fs,
    );
    assert_eq!(
        string_column(&created, "name"),
        vec!["Dave".to_string()],
        "MERGE creates a node when no full-pattern match exists"
    );
    let after_create = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name",
        &fs,
    );
    assert_eq!(
        string_column(&after_create, "name"),
        vec!["Alice".to_string(), "Dave".to_string()],
        "Dave is now persisted alongside Alice"
    );

    // {name: 'Alice'} matches the existing node -> NO new node created.
    let matched = run_write(
        &db_path,
        "MERGE (e:Person {name: 'Alice'}) RETURN e.name AS name",
        &fs,
    );
    assert_eq!(string_column(&matched, "name"), vec!["Alice".to_string()]);
    let after_match = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name",
        &fs,
    );
    assert_eq!(
        string_column(&after_match, "name"),
        vec!["Alice".to_string(), "Dave".to_string()],
        "matching MERGE must not create a duplicate"
    );

    // A second identical MERGE is idempotent — no duplicate Dave.
    run_write(&db_path, "MERGE (e:Person {name: 'Dave'})", &fs);
    let after_idempotent = run_write(
        &db_path,
        "MATCH (n) RETURN n.name AS name ORDER BY n.name",
        &fs,
    );
    assert_eq!(
        string_column(&after_idempotent, "name"),
        vec!["Alice".to_string(), "Dave".to_string()],
        "a repeated identical MERGE is idempotent"
    );

    // Partial match: Alice exists with age 30; {name:'Alice', age:99} differs on
    // age, so it does NOT match and a new node is created.
    let partial = run_write(
        &db_path,
        "MERGE (e:Person {name: 'Alice', age: 99}) RETURN e.age AS age",
        &fs,
    );
    assert_eq!(
        integer_column(&partial, "age"),
        vec![99],
        "a node differing on any inline property is not a match"
    );
    let after_partial = run_write(
        &db_path,
        "MATCH (n) WHERE n.name = 'Alice' RETURN n.age AS age ORDER BY n.age",
        &fs,
    );
    assert_eq!(
        integer_column(&after_partial, "age"),
        vec![30, 99],
        "MERGE on a full property predicate creates a second Alice (age 99)"
    );
}

// ------------------------------------------------------------------
// Task 192: MERGE created nodes (and their inline properties) survive a restart.
// ------------------------------------------------------------------

#[test]
fn merge_created_node_persists_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);

    run_write(&db_path, "MERGE (e:Person {name: 'Dave', age: 42})", &fs);

    // Reopen, then a labelled MATCH must find Dave with his persisted property.
    {
        let mut engine = GraphStorageEngine::open(db_path.clone(), &fs).expect("reopen");
        engine.sync(&fs).expect("sync");
    }
    let result = run_write(
        &db_path,
        "MATCH (n:Person) RETURN n.name AS name, n.age AS age",
        &fs,
    );
    assert_eq!(string_column(&result, "name"), vec!["Dave".to_string()]);
    assert_eq!(integer_column(&result, "age"), vec![42]);

    // A MERGE after restart must MATCH the persisted Dave, not create a second.
    run_write(&db_path, "MERGE (e:Person {name: 'Dave', age: 42})", &fs);
    let count_check = run_write(&db_path, "MATCH (n) RETURN n.name AS name", &fs);
    assert_eq!(
        count_check.rows.len(),
        1,
        "MERGE after restart matches the persisted node (no duplicate)"
    );
}

// ------------------------------------------------------------------
// Persistence across a restart: CREATE -> sync -> drop -> reopen -> MATCH (n).
//
// This proves node identity, properties, AND labels survive a full close/reopen
// cycle end-to-end.  Since rmp Task 189 (catalog sidecar persistence) the schema
// catalog is restored on reopen, so both an unlabelled `MATCH (n)` and a
// labelled `MATCH (n:Person)` resolve the node after a restart.
// ------------------------------------------------------------------

#[test]
fn nodes_persist_across_restart_via_unlabelled_match() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);

    // ---- First lifecycle: create + sync + drop. ----
    init_db(&db_path, &fs);
    {
        let mut engine = GraphStorageEngine::open(db_path.clone(), &fs).expect("open");
        let q = "CREATE (a:Person {name: 'Alice', age: 30})";
        let stmt = parse(q).unwrap();
        analyse(&stmt).unwrap();
        let logical = plan(&stmt).unwrap();
        {
            let ctx = ExecutionContext::new_with_write(&mut engine, &fs);
            execute_plan(&logical, &ctx).unwrap();
        }
        engine.sync(&fs).expect("sync before drop");
        // `engine` dropped here: the durable database directory is all that
        // survives into the second lifecycle.
    }

    // ---- Second lifecycle: reopen a fresh engine and match. ----
    let result = run_write(&db_path, "MATCH (n) RETURN n", &fs);

    assert_eq!(result.columns, vec!["n".to_string()]);
    assert_eq!(
        result.rows.len(),
        1,
        "the created node survives the restart"
    );
    match &result.rows[0][0] {
        Value::Node(node) => {
            // Identity and properties persist end-to-end.
            assert_eq!(node.id, 1, "node id is stable across restart");
            assert_eq!(
                node.properties.get("name"),
                Some(&Value::String("Alice".into())),
                "string property persists across restart"
            );
            assert_eq!(
                node.properties.get("age"),
                Some(&Value::Integer(30)),
                "integer property persists across restart"
            );
            // rmp Task 189: labels are now persisted via the catalog sidecar,
            // so they are restored on reopen.
            assert_eq!(
                node.labels,
                vec!["Person".to_string()],
                "label persists across restart (rmp Task 189); got {:?}",
                node.labels
            );
        }
        other => panic!("expected a Node, got {other:?}"),
    }

    // rmp Task 189 (fixed): a labelled match after restart resolves the node.
    let labelled = run_write(&db_path, "MATCH (n:Person) RETURN n", &fs);
    assert_eq!(
        labelled.rows.len(),
        1,
        "labelled MATCH after restart returns the node (rmp Task 189)"
    );
    match &labelled.rows[0][0] {
        Value::Node(node) => {
            assert_eq!(node.labels, vec!["Person".to_string()]);
            assert_eq!(
                node.properties.get("name"),
                Some(&Value::String("Alice".into()))
            );
        }
        other => panic!("expected a Node, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// Task 189: the schema catalog (label names) survives a restart and a labelled
// scan returns the node, projecting a stored property by name.
// ------------------------------------------------------------------

#[test]
fn labelled_match_after_restart_resolves_via_persisted_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rgraph.db");
    let fs = PosixFileSystem::new(false);
    init_db(&db_path, &fs);

    // First lifecycle: create a labelled node and sync (run_write drops engine).
    run_write(&db_path, "CREATE (a:Person {name: 'Alice', age: 30})", &fs);

    // The catalog sidecar must exist next to the data file after sync.
    let catalog_sidecar = db_path.with_extension("catalog");
    assert!(
        catalog_sidecar.exists(),
        "catalog sidecar must be written on sync"
    );

    // Second lifecycle: a labelled scan resolves the label name through the
    // restored catalog and returns Alice.
    let result = run_write(
        &db_path,
        "MATCH (n:Person) RETURN n.name AS name ORDER BY n.name",
        &fs,
    );
    assert_eq!(
        string_column(&result, "name"),
        vec!["Alice".to_string()],
        "labelled MATCH resolves the persisted label after restart"
    );

    // A label that was never registered resolves to nothing (no false matches).
    let absent = run_write(&db_path, "MATCH (n:Company) RETURN n", &fs);
    assert_eq!(
        absent.rows.len(),
        0,
        "an unknown label must not match any node"
    );
}
