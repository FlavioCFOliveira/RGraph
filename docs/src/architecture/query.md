# Query Execution

> **Note**: This chapter describes the planned architecture.  The Sprint 8
> implementation provides a parser and AST; the physical execution engine
> is scheduled for Sprint 21.

## Pipeline

```text
Cypher Text → Lexer → Parser → AST → Semantic Analyser
                                              ↓
                                   Logical Plan → Physical Plan
                                              ↓
                                   Iterator-based Execution
```

## Logical Plan Operators

| Operator | Description |
|----------|-------------|
| `Scan` | Full scan of nodes or relationships |
| `IndexScan` | Range or point lookup on a B+ tree index |
| `Filter` | Predicate evaluation |
| `Project` | Column selection and expression evaluation |
| `Sort` | `ORDER BY` with spill-to-disk |
| `Limit` | `SKIP` / `LIMIT` |
| `Aggregate` | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` |
| `NestedLoopJoin` | Pattern matching join |
| `HashJoin` | Equi-join for large intermediate sets |

## Physical Operators

Physical operators are iterator-based (`next()`, `open()`, `close()`)
and can be offloaded to the rayon CPU pool for parallelism.

## Planned Optimisations

- **Predicate pushdown** — Move filters as close to the scan as possible.
- **Index intersection** — Combine multiple index scans for conjunctive
  predicates.
- **Cost-based join ordering** — Estimate cardinality from index
  statistics.
