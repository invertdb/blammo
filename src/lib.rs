//! blammo: the write-ahead log for invertdb (Gray and Reuter, 1993). Writes are logged before they're acknowledged,
//! then applied to the structures that hold them; after a crash, replaying the log reapplies whatever those
//! structures hadn't made durable yet.
//!
//! - **Records** are opaque bytes, framed with their length and a CRC-32 (Peterson and Brown, 1961). Replay stops at
//!   the first frame that is short or fails its check: a torn tail from a crash, never acknowledged, and dropped.
//! - **Segments:** the log is a sequence of numbered segment files. [`Log::rotate`] starts a new one.
//! - **Checkpoints** bound the log (as in Mohan et al., 1992, simplified). A checkpoint records, as opaque bytes, the
//!   state the layer above needs before a segment: everything earlier segments did is in it, or durable elsewhere.
//!   Writing one deletes the earlier segments. Recovery reads the latest checkpoint, then replays the segments from
//!   it on.
//!
//! A checkpoint is written to a temporary file and renamed into place, so it either exists whole or not at all.
//! With [`Durability::Full`] (the default), new segments, rotated-out segments and checkpoints are synced, and so is
//! the directory, so they survive a crash of the machine; with [`Durability::Process`] they're written the same way
//! but not synced, for a layer whose records aren't synced either.
//!
//! Taking a checkpoint while writes continue: rotate first, so every later record goes to the new segment; once
//! every record in the earlier segments is applied, capture the state; then write the checkpoint for the new segment.
//! Replay may then apply a record the checkpoint already reflects, so applying must be idempotent (or the layer
//! above must recognize what it already holds).
//!
//! **Telemetry:** spans for opening, replay, sync, rotation and checkpoints (`tracing`), and the metrics
//! `invertdb.log.sync.duration` and `invertdb.log.appended` (the `opentelemetry` API). Both do nothing until the
//! program installs a subscriber and a meter provider.

mod log;

pub use log::{Durability, Log};
