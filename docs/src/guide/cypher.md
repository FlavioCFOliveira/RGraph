# Cypher Reference

RGraph implements the openCypher query language. This chapter documents the
subset supported by each sprint.

## Sprint 8 Subset (Current)

### Clauses

| Clause | Status | Notes |
|--------|--------|-------|
| `MATCH` | ✅ | Fixed-length patterns with labels and types |
| `WHERE` | ✅ | Comparisons, `AND`, `OR`, `NOT` |
| `RETURN` | ✅ | Projections, `ORDER BY`, `SKIP`, `LIMIT` |
| `CREATE` | ✅ | Nodes and relationships with properties |
| `DELETE` | 🚧 | Planned for Sprint 3 |
| `SET` | 🚧 | Planned for Sprint 3 |
| `REMOVE` | 🚧 | Planned for Sprint 3 |
| `MERGE` | 🚧 | Planned for Sprint 3 |

### Expressions

- **Literals**: integers, floats, strings, booleans, `NULL`
- **Arithmetic**: `+`, `-`, `*`, `/`, `%`
- **Comparisons**: `=`, `<>`, `<`, `<=`, `>`, `>=`
- **Logical**: `AND`, `OR`, `NOT`
- **Property access**: `n.name`
- **Lists**: `[1, 2, 3]`
- **Maps**: `{name: 'Alice', age: 30}`

### Data Types

| Type | Rust Equivalent | Storage |
|------|-----------------|---------|
| `Integer` | `i64` | 8 bytes BE |
| `Float` | `f64` | 8 bytes IEEE-754 |
| `String` | `String` | Variable length |
| `Boolean` | `bool` | 1 byte |
| `Null` | `()` | Sentinel |

## Examples

```cypher
// Return a literal
RETURN 42

// Simple arithmetic
RETURN 1 + 2 * 3 AS result

// Match nodes by label
MATCH (n:Person) RETURN n

// Match with filter
MATCH (n:Person) WHERE n.age > 18 RETURN n.name

// Create a node
CREATE (n:Person {name: 'Alice', age: 30})

// Create a relationship
MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
CREATE (a)-[:KNOWS]->(b)
```
