# Benchmarks

This directory contains Criterion benchmarks for RGraph.

## Running

```bash
cargo criterion
```

## Benchmarks

| Benchmark | What it measures |
|-----------|------------------|
| `btree_insert` | B+ tree insertion throughput |
| `btree_search` | B+ tree point-lookup latency |
| `page_insert` | Slotted-page record insertion |
| `graph_create_node` | Node creation throughput |
| `wal_append` | WAL append latency |

## Regression Threshold

A regression is flagged when any benchmark changes by more than **5 %**
compared to the previous nightly run.
