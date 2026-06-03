//! openCypher Technology Compatibility Kit (TCK) compliance harness.
//!
//! This integration-test file provides the scaffold for executing TCK
//! `.feature` scenarios once they are added to the repository.
//!
//! # Current scope (Sprint 21)
//!
//! * Expression-only queries (`RETURN`, literals, arithmetic, lists, maps)
//! * Side-effect tracking structure (stub)
//! * Result table validation helpers
//!
//! # Future work
//!
//! * Integrate the `cucumber` crate to parse `.feature` files.
//! * Add step definitions for `Given`, `When`, `Then`, and side-effect assertions.
//! * Run the full openCypher TCK feature suite in CI.

use rgraph::cypher::executor::execute_expression_query;
use rgraph::cypher::value::Value;
use std::collections::HashMap;

/// Parsed expected result row from a TCK scenario.
#[derive(Debug, Clone)]
struct ExpectedRow {
    columns: HashMap<String, Value>,
}

/// Run a single TCK-style expression query and validate the result.
fn run_tck_expression(query: &str, expected: Vec<ExpectedRow>) {
    let result = execute_expression_query(query).expect("query should execute");

    assert_eq!(
        result.rows.len(),
        expected.len(),
        "row count mismatch for query '{}': got {}, expected {}",
        query,
        result.rows.len(),
        expected.len()
    );

    for (i, (actual_row, expected_row)) in result.rows.iter().zip(expected.iter()).enumerate() {
        for (col_idx, col_name) in result.columns.iter().enumerate() {
            let actual_val = &actual_row[col_idx];
            let expected_val = expected_row
                .columns
                .get(col_name)
                .unwrap_or_else(|| panic!("missing expected column '{}' in row {}", col_name, i));
            assert_eq!(
                actual_val, expected_val,
                "row {} column '{}' mismatch in query '{}'",
                i, col_name, query
            );
        }
    }
}

// ------------------------------------------------------------------
// TCK-style tests for expression queries
// ------------------------------------------------------------------

#[test]
fn tck_return_integer_literal() {
    run_tck_expression(
        "RETURN 42",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("42".to_string(), Value::Integer(42));
                m
            },
        }],
    );
}

#[test]
fn tck_return_arithmetic() {
    run_tck_expression(
        "RETURN 3 + 4 * 2",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("(3 + (4 * 2))".to_string(), Value::Integer(11));
                m
            },
        }],
    );
}

#[test]
fn tck_return_null_arithmetic() {
    run_tck_expression(
        "RETURN NULL + 5",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("(NULL + 5)".to_string(), Value::Null);
                m
            },
        }],
    );
}

#[test]
fn tck_return_string_concatenation() {
    run_tck_expression(
        "RETURN 'hello' + ' ' + 'world'",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert(
                    "(('hello' + ' ') + 'world')".to_string(),
                    Value::String("hello world".to_string()),
                );
                m
            },
        }],
    );
}

#[test]
fn tck_return_boolean_not() {
    run_tck_expression(
        "RETURN NOT TRUE",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("NOT true".to_string(), Value::Boolean(false));
                m
            },
        }],
    );
}

#[test]
fn tck_return_list_literal() {
    run_tck_expression(
        "RETURN [1, 2, 3]",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert(
                    "[1, 2, 3]".to_string(),
                    Value::List(vec![
                        Value::Integer(1),
                        Value::Integer(2),
                        Value::Integer(3),
                    ]),
                );
                m
            },
        }],
    );
}

#[test]
fn tck_return_map_literal() {
    run_tck_expression(
        "RETURN {a: 1, b: 'two'}",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                let mut inner = HashMap::new();
                inner.insert("a".to_string(), Value::Integer(1));
                inner.insert("b".to_string(), Value::String("two".to_string()));
                m.insert("{a: 1, b: 'two'}".to_string(), Value::Map(inner));
                m
            },
        }],
    );
}

#[test]
fn tck_return_empty_list() {
    run_tck_expression(
        "RETURN []",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("[]".to_string(), Value::List(vec![]));
                m
            },
        }],
    );
}

#[test]
fn tck_return_comparison() {
    run_tck_expression(
        "RETURN 5 > 3",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("(5 > 3)".to_string(), Value::Boolean(true));
                m
            },
        }],
    );
}

#[test]
fn tck_return_equality_with_null() {
    run_tck_expression(
        "RETURN 5 = NULL",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("(5 = NULL)".to_string(), Value::Null);
                m
            },
        }],
    );
}

#[test]
fn tck_return_is_null() {
    run_tck_expression(
        "RETURN NULL IS NULL",
        vec![ExpectedRow {
            columns: {
                let mut m = HashMap::new();
                m.insert("NULL IS NULL".to_string(), Value::Boolean(true));
                m
            },
        }],
    );
}
