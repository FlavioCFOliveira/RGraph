# Quick Start

## 1. Initialise a Database

```bash
rgraph init /var/lib/rgraph/mydb
```

This creates a new database directory with the superblock, page bitmap,
and WAL directory.

## 2. Start the Server

```bash
rgraph serve /var/lib/rgraph/mydb
```

The server listens on `0.0.0.0:7687` by default and exposes:

- **gRPC** on the same port
- `/health` — liveness probe
- `/ready` — readiness probe
- `/metrics` — Prometheus text exposition

## 3. Execute a Cypher Query (gRPC)

Using the generated client:

```rust
use rgraph::server::grpc::proto::cypher_query_client::CypherQueryClient;
use rgraph::server::grpc::proto::QueryRequest;

#[tokio::main]
async fn main() {
    let mut client = CypherQueryClient::connect("http://localhost:7687")
        .await
        .unwrap();
    let req = tonic::Request::new(QueryRequest {
        query: "RETURN 1 + 2 AS result".into(),
        parameters: Default::default(),
    });
    let resp = client.execute(req).await.unwrap();
    println!("{:?}", resp.into_inner());
}
```

## 4. Stop the Server

Send `SIGTERM` or `SIGINT`:

```bash
kill -TERM $(pgrep rgraph)
```

The server performs a graceful shutdown: it stops accepting new connections,
waits for in-flight transactions to complete (up to a bounded timeout),
flushes the WAL, and exits cleanly.
