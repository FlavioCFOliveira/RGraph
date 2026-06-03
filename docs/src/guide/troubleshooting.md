# Troubleshooting

## Common Issues

### "bind failed: Address already in use"

Another process is listening on the configured port. Either stop the
conflicting process or start RGraph on a different port:

```bash
rgraph serve /var/lib/rgraph/mydb --port 7688
```

### "database path already exists"

The `init` command refuses to overwrite an existing directory. If you are
certain you want to recreate the database, remove the directory first:

```bash
rm -rf /var/lib/rgraph/mydb
rgraph init /var/lib/rgraph/mydb
```

### "invalid primary superblock"

The database may be corrupted or was created by an incompatible version.
Check the mirror superblock copy; RGraph automatically uses the newer of the
two copies on open. If both are invalid, recovery is not possible without
a backup.

### High Memory Usage

If the buffer pool grows too large, reduce the number of frames or enable
more aggressive background flushing. These settings are configured at
compile time in `src/buffer/pool.rs`.

### Slow Queries

1. Check the query plan — complex `MATCH` patterns without index support
   will result in full scans.
2. Ensure secondary indexes (label, type, property) are populated.
3. Monitor `rgraph_cache_hit_rate`; a low value indicates buffer pool
   pressure.

## Getting Help

- Open an issue on [GitHub](https://github.com/FlavioCFOliveira/RGraph/issues)
- Include the output of `rgraph --version` and the relevant log lines
  (run with `-vv` for TRACE-level output)
