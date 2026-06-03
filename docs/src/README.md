# RGraph User Guide

RGraph is a high-performance graph database engine written in Rust, designed
from the ground up for environments of extreme concurrency and load. It
supports both the **Label Property Graph (LPG)** and **RDF** data models and
is built to be **100% openCypher compliant** and **100% ACID compliant**.

## Operating Modes

RGraph can operate in two distinct modes:

1. **One-shot** — Direct invocation from a CLI tool or script for batch
   processing and administrative tasks.
2. **Server** — A continuously running gRPC/Bolt-compatible service
   handling thousands of concurrent connections with predictable latency.

## Key Features

- **Custom storage engine** — Native B+ tree indexes, slotted pages, and a
  user-space buffer pool (CLOCK-Pro) built specifically for graph access
  patterns.
- **Full ACID transactions** — Snapshot-isolation MVCC with wound-wait
  deadlock prevention and ARIES-style crash recovery.
- **Dual model support** — LPG (nodes, relationships, properties) and RDF
  (triples, quads) over a unified storage layer.
- **Production observability** — Prometheus metrics, structured tracing, and
  health/readiness endpoints.

## Quick Links

- [Installation](guide/installation.md)
- [Quick Start](guide/quickstart.md)
- [Cypher Reference](guide/cypher.md)
- [Architecture Overview](architecture/overview.md)
