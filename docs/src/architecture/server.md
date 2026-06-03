# Server Mode

The server mode introduced in Sprint 23 transforms RGraph from a CLI tool
into a production network service.

## Async Runtime

A multi-threaded Tokio runtime is created with a configurable number of
worker threads (default: `num_cpus`).  CPU-bound work (query planning,
B+ tree traversal, analytics) is offloaded to a dedicated rayon pool via
`tokio::task::spawn_blocking` so the async runtime remains responsive.

## Graceful Shutdown

On `SIGTERM` or `SIGINT`:

1. The `shutting_down` atomic flag is set.
2. A broadcast channel notifies all worker tasks.
3. The connection acceptor stops accepting new connections.
4. In-flight transactions are allowed to complete up to a bounded
   timeout (default: 30 s).
5. The WAL is flushed.
6. The runtime exits.

## Protocol Support

### gRPC (HTTP/2)

The primary protocol.  Services defined in `proto/rgraph.proto`:

- `CypherQuery` — `Execute`, `ExecuteStream`
- `TransactionManager` — `Begin`, `Commit`, `Rollback`
- `Health` — `Check`, `Ready`, `Metrics`

### Bolt (Future)

The `ProtocolMultiplexer` detects the Bolt handshake magic bytes
(`0x60 0x60 0xB0 0x17`) and will route to a Bolt handler once
implemented.

## Backpressure

A `tokio::sync::Semaphore` limits the number of concurrent connections.
When the limit is reached, new connections are rejected gracefully.
Bounded channels prevent memory exhaustion under overload.
