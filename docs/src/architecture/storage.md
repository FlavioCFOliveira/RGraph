# Storage Engine

The storage engine is the lowest layer of RGraph.  It provides durable,
page-addressable storage on top of a custom B+ tree and slotted-page
format.

## Page Layout

Each page is **8 KB** (`PAGE_SIZE = 8192`).  The first two pages are
reserved:

| Page | Purpose |
|------|---------|
| 0 | Primary superblock |
| 1 | Mirror superblock + allocation bitmap |

Data pages start at page 2 and use the **slotted page** format:

```text
┌─────────────────────────────────────────────┐
│ Header (16 bytes)                           │
│   slot_count, free_offset, flags, checksum  │
├─────────────────────────────────────────────┤
│ Slot Directory (4 bytes × N)                │
│   offset, length, flags                     │
├─────────────────────────────────────────────┤
│ Free Space                                  │
├─────────────────────────────────────────────┤
│ Records (inserted from the bottom up)       │
└─────────────────────────────────────────────┘
```

## B+ Tree

The B+ tree is the primary indexing structure:

- **Node size**: one page (8 KB)
- **Key format**: composite encoded (`label_index_key`, `type_index_key`,
  `property_index_key`)
- **Concurrency**: latch crabbing with optimistic reads and
  `crossbeam-epoch` reclamation
- **Split/Merge**: standard B+ tree algorithms with page pinning

## Write-Ahead Log

All mutations are logged before they touch the buffer pool:

| Record Type | Payload |
|-------------|---------|
| `PageUpdate` | page_id + page image |
| `NodeInsert` | node_id + label_id + slot_ref |
| `EdgeInsert` | edge_id + type_id + src + tgt + slot_ref |
| `Commit` | txid + LSN |

Recovery follows the **ARIES** protocol: analysis, redo, undo.
