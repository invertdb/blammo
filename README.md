# blamo

The write-ahead log for invertdb: writes are logged before they're acknowledged, then applied; after a crash,
replaying the log reapplies whatever hadn't been made durable yet (Gray and Reuter, 1993).

```rust
let log = blamo::Log::open("collection/log")?;
if let Some(state) = log.checkpoint() { /* restore state */ }
log.replay(|record| { /* reapply */ Ok(()) })?;

log.append(b"record")?;                    // or append_all for a batch in one write
log.sync()?;                               // durable: now acknowledge

// after a commit makes everything logged durable elsewhere:
log.reset(b"state")?;

// a checkpoint while writes continue:
let segment = log.rotate()?;               // later records go to a new segment
/* once earlier records are applied, capture the state */
log.write_checkpoint(segment, &state)?;    // deletes the earlier segments
```

## Model

- **Records** are opaque bytes, framed with their length and a CRC-32 (Peterson and Brown, 1961). Replay stops at
  the first frame that is short or fails its check: a torn tail from a crash, never acknowledged, and dropped.
  Damage before the last segment's end is an error.
- **Segments** are numbered files. `rotate` starts a new one.
- **Checkpoints** bound the log (Mohan et al., 1992, simplified): opaque state that holds before a segment.
  Writing one deletes the earlier segments. Recovery reads the latest checkpoint, then replays the segments from it
  on.
  - A checkpoint is written to a temporary file, synced and renamed into place, so it exists whole or not at all.
  - New files are made durable by syncing the directory.
- **Checkpointing while writes continue** (rotate, apply, capture, write) can replay a record the checkpoint
  already reflects, so applying must be idempotent, or the layer above must recognize what it already holds.

## Why a library of its own

Every invertdb service that acknowledges writes before committing them needs a log, and in a cluster the log
becomes the replicated commit log. That needs replication strategies different from the B+tree
([b-positive](https://github.com/invertdb/b-positive)) and blobs ([larry](https://github.com/invertdb/larry)).

## Tests

`cargo test`:
- Records survive reopening, in order.
- A torn tail is dropped, and appending continues after the valid frames.
- A corrupt record ends the last segment but is an error in an earlier one.
- Checkpoints delete earlier segments and survive reopening; stale or unstarted checkpoints are refused.
- An interrupted checkpoint leaves the previous one.

## References

- Gray, J., and Reuter, A. (1993). *Transaction Processing: Concepts and Techniques*. Morgan Kaufmann.
- Mohan, C., Haderle, D., Lindsay, B., Pirahesh, H., and Schwarz, P. (1992). ARIES: a transaction recovery method
  supporting fine-granularity locking and partial rollbacks using write-ahead logging. *ACM Transactions on Database
  Systems* 17(1).
- Peterson, W. W., and Brown, D. T. (1961). Cyclic codes for error detection. *Proceedings of the IRE* 49(1).
