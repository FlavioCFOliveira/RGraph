# gRPC API

The gRPC service is defined in `proto/rgraph.proto`.

## CypherQuery

### Execute

```protobuf
rpc Execute(QueryRequest) returns (QueryResponse);
```

**QueryRequest**

| Field | Type | Description |
|-------|------|-------------|
| `query` | `string` | Cypher query text |
| `parameters` | `map<string, Value>` | Named parameters |

**QueryResponse**

| Field | Type | Description |
|-------|------|-------------|
| `result_set` | `ResultSet` | Successful result |
| `error` | `ErrorPayload` | Execution error |

**ResultSet**

| Field | Type | Description |
|-------|------|-------------|
| `columns` | `repeated string` | Column names |
| `rows` | `repeated Row` | Result rows |

## TransactionManager

| RPC | Request | Response |
|-----|---------|----------|
| `Begin` | `BeginRequest` | `BeginResponse` (tx_id) |
| `Commit` | `CommitRequest` | `CommitResponse` |
| `Rollback` | `RollbackRequest` | `RollbackResponse` |

## Health

| RPC | Path | Description |
|-----|------|-------------|
| `Check` | `/health` | Liveness probe |
| `Ready` | `/ready` | Readiness probe |
| `Metrics` | `/metrics` | Prometheus text format |
