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
//! Every assertion below covers behaviour that is verified to work end-to-end
//! today.  Three known gaps are deliberately **not** asserted as passing, each
//! tracked by its own rmp task so this suite stays honest:
//!
//! * **Labelled MATCH after restart** (`MATCH (n:Person)`): the in-memory schema
//!   catalog (label/type/property-key name -> id map) is rebuilt empty on
//!   reopen, so a labelled scan finds nothing after a restart even though the
//!   node persists and an unlabelled `MATCH (n)` still finds it.  Tracked by
//!   **rmp Task 189** ("Persist the schema catalog to disk and restore it on
//!   open").  The restart test below therefore asserts node identity and
//!   properties via an *unlabelled* match, not labels.
//!
//! * **End-to-end aggregation** (`RETURN count(n)`, `sum`/`avg`/`min`/`max`):
//!   the aggregation operator does not see MATCH-bound pattern variables against
//!   the real engine pipeline.  Tracked by **rmp Task 190**.  No aggregation
//!   result is asserted here.
//!
//! * **`SET` write-back across statements**: a `SET n.prop = v` is reflected in
//!   the mutating statement's own RETURN projection but is not flushed to the
//!   node's record, so a subsequent separate MATCH reads the pre-SET value.
//!   Surfaced for its own task; this suite does not assert `SET` read-back.

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
// MERGE is NOT asserted here.
//
// Verified during Task 185: against the real engine pipeline `MERGE (e:Person
// {name: 'X'})` ignores the inline property predicate and matches the first
// label-compatible node, so it never creates a node for an absent key and never
// filters by properties (e.g. `MERGE (e:Person {name: 'Dave'})` against a graph
// holding only Alice returns Alice instead of creating Dave).  Because MERGE
// read-back does not work end-to-end, no MERGE behaviour is asserted as passing.
// This gap is surfaced for its own rmp task (sibling to Tasks 189 and 190) and
// will get a dedicated end-to-end MERGE test once the operator binds and honours
// the pattern's property predicate.
// ------------------------------------------------------------------

// ------------------------------------------------------------------
// Persistence across a restart: CREATE -> sync -> drop -> reopen -> MATCH (n).
//
// This proves node identity and properties survive a full close/reopen cycle
// end-to-end.  The match is deliberately *unlabelled* (`MATCH (n)`): the schema
// catalog is not yet persisted, so labels are lost on reopen and a labelled
// `MATCH (n:Person)` would return zero rows after restart — that gap is tracked
// by rmp Task 189 and is asserted only as a documented expectation here, never
// as passing behaviour.
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
            // NOTE (rmp Task 189): labels are NOT persisted yet — the schema
            // catalog is rebuilt empty on reopen, so `node.labels` is empty here
            // and a labelled `MATCH (n:Person)` returns zero rows after restart.
            // We assert the *current* documented behaviour (no labels) rather
            // than the desired one, so this test is honest about the gap.
            assert!(
                node.labels.is_empty(),
                "labels are currently lost on reopen (rmp Task 189); got {:?}",
                node.labels
            );
        }
        other => panic!("expected a Node, got {other:?}"),
    }

    // Confirm the Task 189 gap explicitly: a labelled match after restart finds
    // nothing.  Documented here (not asserted as a feature) so a future fix that
    // makes this return the node will flip this assertion and flag the test for
    // update alongside Task 189.
    let labelled = run_write(&db_path, "MATCH (n:Person) RETURN n", &fs);
    assert_eq!(
        labelled.rows.len(),
        0,
        "labelled MATCH after restart currently returns 0 rows (rmp Task 189)"
    );
}
