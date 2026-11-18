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
Each production profile has one executor process, protected by an OS file lock
retained by all of its journal clones. After that process exits, the next owner
atomically reclaims orphaned work immediately rather than waiting for the old
lease deadline. Control connections can inspect and authorize work while an
executor is active; they do not execute filesystem or network effects.

Namespace identity, content availability, and local modifications are separate
concepts. A placeholder can satisfy metadata availability while an eager folder
download requires complete contents. CFAPI hydration requests retain their own
deadline-sensitive callback workers.

The folder agent journals remote materialization in `sync-work.sqlite` next to
its baseline. Continuous sync executes at most 64 jobs per pass and retries at
the journal's next eligible deadline without waiting for another remote change.
An empty journal does not add a retry timer. One-shot sync
drains ready work and returns an error if a transfer fails. Pending startup
preservation decisions and local file/subtree fingerprints survive restart;
changed local bytes become conflicts instead of being overwritten on retry.
Folder uploads and remote mutations still use their existing path-based runtime;
this integration does not change that protocol to object-ID mutations.

Authoritative snapshots schedule folder work; retry passes only drain persisted
jobs. They never infer deletions from a cached snapshot or replace upload-known
remote state. Unchanged completed jobs retain their original payload, so an
unrelated remote update cannot recreate a local deletion before it is observed.
An unchanged remote echo is recognized against the persisted synced content
fingerprint (or directory kind). Local edits, deletions and type replacements
remain for the local executor rather than becoming blocked remote conflicts.
A genuinely changed remote child under a missing or replaced local directory
still requires conflict resolution; it cannot recreate that directory over the
user's replacement.
Successful per-path baselines are persisted incrementally; backed-off work does
not refresh the last-success timestamp or rewrite the whole baseline.

Dependency readiness is normalized into indexed due-work rows. Blocked descendants
and completed rows are absent from the claim index. Terminal history outside the
current desired plan can be pruned while retaining active prerequisites and current
desired completions. Directory preconditions share one tree capture per planning
or execution batch rather than walking the entire root for every directory.

Folder queue conflicts appear in the existing conflict list and resolution API.
An explicit keep-remote choice preserves the local file or directory before the
queued remote operation resumes. Keep-local runs the existing folder transfer or
delete executor for the chosen path. Editing a conflicted path alone never grants
permission to overwrite it. Backends must implement conflict-copy preservation to
support keep-remote; unsupported preservation fails without replacing local bytes.
Local scans retain the previous baseline for paths blocked by remote work, avoiding
upload/delete echoes while resolution is pending. Normal scheduling checks terminal
retention at most once per day and retains seven days of historical terminal jobs,
in addition to current desired completions and active prerequisites.

CFAPI uses the same journal under its local application-data sync-root state,
scoped by provider instance, registered cluster identity, and normalized remote
prefix. Endpoint labels and same-cluster failover do not select another journal. Startup first
compares actual placeholders with persisted observations and records local
deletions/moves. Existing roots with no journal are seeded from the filesystem;
there is no legacy scheduler switch or compatibility runtime.

Remote materialization is grouped by parent into batches of at most 64 entries.
The worker executes at most eight batches per turn, between five-second local
topology scans. It sleeps until the next eligible job or topology scan; scheduling
and accepted upload receipts wake the same worker without database polling.
External conflict controls are observed within the topology scan interval.
Missing previously observed paths wait for a topology scan so
background population cannot recreate an unobserved user deletion or rename.
Partial batch success is recorded per path before acknowledgement; a failed
first entry does not prevent independent siblings from being attempted.

Deletes and moves retain object ID and expected revision. Accepted mutations
leave revision fences so a snapshot fetched before replay cannot recreate or
revert the old revision. Newer remote restores remain eligible. Upload bytes
still use the guarded CFAPI transfer executor; its shared uploader boundary
journals upload intent and accepted receipts before returning success. Receipt
repair is separate durable work and advances only the upload's own source
identity, preserving local bytes that changed during transfer. Newly accepted
object identity is retained even when the file disappears before the first scan.
Directory-marker deletion is nonrecursive and succeeds only when the server's
live namespace has no descendants. A new or unseen child causes a mutation
conflict (HTTP409); children are never swept away by an automatic recursive retry.
Provider threads serialize each path through the entire close-upload finalizer,
including its receipt, snapshot and USN checks. Receipt repair waits for that
boundary and re-inspects identity before writing; it cannot invalidate a USN
captured by the callback. These gates do not block user writes, which remain
protected by the snapshot/USN checks.

CFAPI persists observation deltas rather than rewriting the whole tree. Negative
directory attribute hints are verified with metadata-only placeholder inspection;
a cold placeholder is never treated as an ordinary upload merely because cached
enumeration attributes omit its placeholder flag. Existing status surfaces show
pending work and conflicts instead of announcing completion after scheduling.
This adds a metadata probe per visited entry intentionally: file size, timestamps
and previous classification cannot prove that a zero-length file has not become
a placeholder. Probe failures defer that entry with an observable diagnostic;
only known ordinary-file errors, or INVALID_FUNCTION with an independently
proven unregistered root and negative attribute hint, permit ordinary-file work.
