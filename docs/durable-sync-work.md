# Durable sync work

`sync-agent-core::sync_work` owns the operation lifecycle shared by filesystem
adapters. Its SQLite journal lives in adapter-owned profile state, outside the
synced tree. WAL with `synchronous=FULL` persists intent before observations can
advance. A completed side effect and a SQLite commit are separate events:
executors must inspect actual identity and revision before replaying work.

Desired remote materialization replaces obsolete work. Local mutation intents
are enqueued separately and are never cancelled merely because a remote snapshot
does not contain their paths. Parent dependencies gate descendants; transient
failures back off, and conflicts stay visible without automatic destructive retry.

Claims and resource leases are transactional. Superseding an in-flight generation
does not release its resource until that execution finishes or its lease expires.
Generation and claim tokens reject stale acknowledgements. Long operations renew
their leases; opening another connection does not steal live work.

Namespace identity, content availability, and local modifications are separate
concepts. A placeholder can satisfy metadata availability while an eager folder
download requires complete contents. CFAPI hydration requests retain their own
deadline-sensitive callback workers.

The folder agent journals remote materialization in `sync-work.sqlite` next to
its baseline. Continuous sync executes at most 64 jobs per pass and retries on
a one-second timer without waiting for another remote change. One-shot sync
drains ready work and returns an error if a transfer fails. Pending startup
preservation decisions and local file/subtree fingerprints survive restart;
changed local bytes become conflicts instead of being overwritten on retry.
Folder uploads and remote mutations still use their existing path-based runtime;
this integration does not change that protocol to object-ID mutations.
