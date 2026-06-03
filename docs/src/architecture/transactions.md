# Transactions

RGraph implements **snapshot isolation** using a PostgreSQL-style MVCC
design.

## Tuple Header

Every record carries a 16-byte header:

```rust
#[repr(C)]
struct TupleHeader {
    xmin: u32,      // creating transaction
    xmax: u32,      // deleting transaction (0 = alive)
    cid: u16,       // command id within transaction
    infomask: u16,  // cached commit/abort flags
    next_version: u32, // pointer to newer version
}
```

## Visibility Rules

A tuple is visible to a snapshot if:

1. `xmin` is committed and `xmin < snapshot.xmin` or `xmin` is not in
   the active set.
2. `xmax` is either 0, aborted, or `xmax >= snapshot.xmax`.

## Locking

| Lock Mode | Compatibility |
|-----------|---------------|
| IS (Intention Shared) | Compatible with IS, S |
| IX (Intention Exclusive) | Compatible with IX |
| S (Shared) | Compatible with IS, S |
| X (Exclusive) | Exclusive only |

Deadlock prevention uses the **wound-wait** algorithm:

- Older transactions wound (abort) younger conflicting holders.
- Younger transactions wait for older holders.
- A transaction wounded three times gains immunity.

## Durability

A commit is acknowledged only after:

1. All WAL records for the transaction are written.
2. `WalWriter::sync()` returns successfully.
3. The commit record is durable on disk.
