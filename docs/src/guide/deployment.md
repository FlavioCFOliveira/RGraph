# Deployment

## System Requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 2 cores | 8+ cores |
| RAM | 4 GB | 32 GB |
| Disk | SSD | NVMe SSD |
| OS | Linux 5.15+ | Linux 6.x |

## systemd Service

Create `/etc/systemd/system/rgraph.service`:

```ini
[Unit]
Description=RGraph Database Server
After=network.target

[Service]
Type=notify
ExecStart=/usr/local/bin/rgraph serve /var/lib/rgraph/mydb
Restart=on-failure
RestartSec=5
User=rgraph
Group=rgraph
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl enable rgraph
sudo systemctl start rgraph
```

## TLS

Generate a self-signed certificate for testing:

```bash
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem \
  -days 365 -nodes -subj "/CN=localhost"
```

Run with TLS:

```bash
rgraph serve /var/lib/rgraph/mydb --tls --tls-cert cert.pem --tls-key key.pem
```

## Monitoring

### Prometheus

Scrape `http://localhost:7687/metrics` (or the configured host:port).

Key metrics:

- `rgraph_queries_total` — total queries executed
- `rgraph_query_latency_seconds` — query latency histogram
- `rgraph_active_connections` — current open connections
- `rgraph_cache_hit_rate` — buffer pool hit rate

### Health Checks

- **Liveness**: `GET /health` → `200` when the process is running
- **Readiness**: `GET /ready` → `200` when the database is ready to serve
