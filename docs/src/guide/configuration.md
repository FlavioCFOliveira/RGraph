# Configuration

RGraph is configured primarily through CLI flags and environment variables.
No external configuration file is required for basic operation.

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `RGRAPH_LOG` | Log filter directive (e.g. `debug`, `rgraph::server=trace`) | `info` |
| `RUST_LOG` | Fallback log filter | `info` |

## Server Mode

When running `rgraph serve`, the following flags control behaviour:

| Flag | Description | Default |
|------|-------------|---------|
| `--host` | Bind address | `0.0.0.0` |
| `--port` | Bind port | `7687` |
| `--workers` | Tokio worker threads | `num_cpus` |
| `--cpu-threads` | Rayon CPU pool threads | `num_cpus` |
| `--max-connections` | Maximum concurrent connections | `1024` |
| `--tls` | Enable TLS | `false` |
| `--tls-cert` | Path to TLS certificate (PEM) | — |
| `--tls-key` | Path to TLS private key (PEM) | — |

## Logging Verbosity

All subcommands accept:

- `-v` / `--verbose` — Increase verbosity (repeat for `trace`)
- `-q` / `--quiet` — Suppress all output below `error`
- `--json-log` — Emit structured JSON log lines (ideal for log aggregation)
