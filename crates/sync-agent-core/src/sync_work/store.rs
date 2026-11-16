//! Durable, adapter-independent operation lifecycle. Filesystem effects are outside
//! SQLite transactions: replaying executors verify actual identity and revision.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedWork {
    pub key: String,
    pub resource: String,
    pub payload: Value,
    pub depends_on: Vec<String>,
}
impl PlannedWork {
    pub fn new(key: impl Into<String>, resource: impl Into<String>, payload: Value) -> Self {
        Self {
            key: key.into(),
            resource: resource.into(),
            payload,
            depends_on: Vec::new(),
        }
    }
}

/// Availability is independent of namespace identity and locally modified bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentAvailability {
    MetadataOnly,
    Partial,
    Complete,
}
#[derive(Clone, Debug)]
pub struct WorkLease {
    pub scope: String,
    pub work: PlannedWork,
    generation: i64,
    token: i64,
    attempts: u32,
}
#[derive(Clone, Debug)]
pub enum WorkResult {
    Complete,
    Retry(String),
    Conflict(String),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkStatus {
    pub pending: u64,
    pub running: u64,
    pub conflicts: u64,
    pub completed: u64,
}
#[derive(Clone)]
pub struct WorkStore {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

// Only eligible work is indexed here. Completed jobs and blocked descendants are
// absent, so claiming a large tree does not repeatedly scan its completed prefix.
const CLAIM_SQL:&str="SELECT w.key,w.resource,w.payload,w.dependencies,w.generation,w.token,w.attempts
    FROM sync_work_ready r INDEXED BY sync_work_due
    JOIN sync_work w ON w.scope=r.scope AND w.key=r.key
    WHERE r.scope=?1 AND r.available_at<=?2 AND (?3 IS NULL OR r.key=?3)
      AND NOT EXISTS(SELECT 1 FROM sync_work_resources a WHERE a.resource=w.resource AND a.lease_until>?2)
    ORDER BY r.available_at,r.key LIMIT 1";

impl WorkStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("opening sync work journal {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS sync_work(
                scope TEXT NOT NULL,key TEXT NOT NULL,resource TEXT NOT NULL,payload TEXT NOT NULL,dependencies TEXT NOT NULL,
                generation INTEGER NOT NULL DEFAULT 1,token INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,available_at INTEGER NOT NULL DEFAULT 0,lease_until INTEGER NOT NULL DEFAULT 0,
                error TEXT,finished_at INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scope,key));
            CREATE INDEX IF NOT EXISTS sync_work_status ON sync_work(scope,state);
            CREATE INDEX IF NOT EXISTS sync_work_resource ON sync_work(resource,scope,key);
            CREATE TABLE IF NOT EXISTS sync_work_resources(resource TEXT PRIMARY KEY,scope TEXT NOT NULL,key TEXT NOT NULL,
                generation INTEGER NOT NULL,token INTEGER NOT NULL,lease_until INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS sync_work_dependencies(scope TEXT NOT NULL,key TEXT NOT NULL,dependency TEXT NOT NULL,
                PRIMARY KEY(scope,key,dependency),FOREIGN KEY(scope,key) REFERENCES sync_work(scope,key) ON DELETE CASCADE);
            CREATE INDEX IF NOT EXISTS sync_work_dependents ON sync_work_dependencies(scope,dependency,key);
            CREATE TABLE IF NOT EXISTS sync_work_ready(scope TEXT NOT NULL,key TEXT NOT NULL,available_at INTEGER NOT NULL,
                PRIMARY KEY(scope,key),FOREIGN KEY(scope,key) REFERENCES sync_work(scope,key) ON DELETE CASCADE);
            CREATE INDEX IF NOT EXISTS sync_work_due ON sync_work_ready(scope,available_at,key);
            CREATE TABLE IF NOT EXISTS sync_work_desired(scope TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(scope,key),
                FOREIGN KEY(scope,key) REFERENCES sync_work(scope,key) ON DELETE CASCADE);
            CREATE TABLE IF NOT EXISTS sync_observations(scope TEXT PRIMARY KEY,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sync_path_observations(scope TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(scope,path));")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            let has_finished = connection
                .prepare("PRAGMA table_info(sync_work)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .any(|name| name == "finished_at");
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !has_finished {
                tx.execute(
                    "ALTER TABLE sync_work ADD COLUMN finished_at INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            tx.execute("INSERT OR IGNORE INTO sync_work_dependencies(scope,key,dependency) SELECT w.scope,w.key,d.value FROM sync_work w,json_each(w.dependencies) d",[])?;
            tx.execute_batch("DELETE FROM sync_work_ready;
                INSERT INTO sync_work_ready(scope,key,available_at)
                SELECT w.scope,w.key,CASE WHEN w.state='running' THEN w.lease_until ELSE max(w.available_at,coalesce(a.lease_until,0)) END
                FROM sync_work w LEFT JOIN sync_work_resources a ON a.resource=w.resource
                WHERE w.state IN ('pending','running') AND NOT EXISTS(
                    SELECT 1 FROM sync_work_dependencies d LEFT JOIN sync_work p ON p.scope=d.scope AND p.key=d.dependency
                    WHERE d.scope=w.scope AND d.key=w.key AND (p.state IS NULL OR p.state<>'complete'));
                PRAGMA user_version=2;")?;
            tx.commit()?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: path.to_path_buf(),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn enqueue(&self, scope: &str, work: &[PlannedWork]) -> Result<()> {
        self.write_plan(scope, work, false)
    }
    /// Supply a complete authoritative plan, never a partial filesystem scan.
    pub fn reconcile(&self, scope: &str, work: &[PlannedWork]) -> Result<()> {
        self.write_plan(scope, work, true)
    }
    fn write_plan(&self, scope: &str, work: &[PlannedWork], replace: bool) -> Result<()> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if replace {
            tx.execute("DELETE FROM sync_work_desired WHERE scope=?1", [scope])?;
        }
        for item in work {
            if item.key.is_empty()
                || item.resource.is_empty()
                || item.depends_on.contains(&item.key)
            {
                bail!("sync work requires a key/resource and cannot depend on itself");
            }
            let changed=tx.execute("INSERT INTO sync_work(scope,key,resource,payload,dependencies) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(scope,key) DO UPDATE SET resource=excluded.resource,payload=excluded.payload,dependencies=excluded.dependencies,
                generation=sync_work.generation+1,token=sync_work.token+1,state='pending',attempts=0,available_at=0,lease_until=0,error=NULL,finished_at=0
                WHERE sync_work.payload<>excluded.payload OR sync_work.dependencies<>excluded.dependencies OR sync_work.resource<>excluded.resource OR sync_work.state='cancelled'",
                params![scope,item.key,item.resource,serde_json::to_string(&item.payload)?,serde_json::to_string(&item.depends_on)?])?;
            if changed != 0 {
                tx.execute(
                    "DELETE FROM sync_work_dependencies WHERE scope=?1 AND key=?2",
                    params![scope, item.key],
                )?;
                for dependency in &item.depends_on {
                    tx.execute("INSERT OR IGNORE INTO sync_work_dependencies(scope,key,dependency) VALUES(?1,?2,?3)",params![scope,item.key,dependency])?;
                }
                refresh_ready(&tx, scope, &item.key)?;
                refresh_dependents(&tx, scope, &item.key)?;
            }
            if replace {
                tx.execute(
                    "INSERT INTO sync_work_desired(scope,key) VALUES(?1,?2)",
                    params![scope, item.key],
                )?;
            }
        }
        if replace {
            tx.execute("UPDATE sync_work SET state='cancelled',token=token+1,finished_at=?2 WHERE scope=?1 AND state<>'cancelled' AND NOT EXISTS(SELECT 1 FROM sync_work_desired d WHERE d.scope=sync_work.scope AND d.key=sync_work.key)",params![scope,now_ms()])?;
            tx.execute("DELETE FROM sync_work_ready WHERE scope=?1 AND (EXISTS(SELECT 1 FROM sync_work w WHERE w.scope=sync_work_ready.scope AND w.key=sync_work_ready.key AND w.state NOT IN ('pending','running')) OR EXISTS(SELECT 1 FROM sync_work_dependencies d LEFT JOIN sync_work p ON p.scope=d.scope AND p.key=d.dependency WHERE d.scope=sync_work_ready.scope AND d.key=sync_work_ready.key AND (p.state IS NULL OR p.state<>'complete')))",[scope])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn claim_next(&self, scope: &str, now: i64, lease_ms: i64) -> Result<Option<WorkLease>> {
        self.claim_matching(scope, None, now, lease_ms)
    }
    pub fn claim_key(
        &self,
        scope: &str,
        key: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<WorkLease>> {
        self.claim_matching(scope, Some(key), now, lease_ms)
    }
    fn claim_matching(
        &self,
        scope: &str,
        key: Option<&str>,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<WorkLease>> {
        if lease_ms <= 0 {
            bail!("sync work lease must be positive");
        }
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = tx
            .query_row(CLAIM_SQL, params![scope, now, key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, u32>(6)?,
                ))
            })
            .optional()?;
        let Some((key, resource, payload, dependencies, generation, token, attempts)) = candidate
        else {
            return Ok(None);
        };
        let token = token + 1;
        let until = now.saturating_add(lease_ms);
        tx.execute("UPDATE sync_work SET state='running',token=?3,attempts=attempts+1,lease_until=?4 WHERE scope=?1 AND key=?2",params![scope,key,token,until])?;
        tx.execute("INSERT INTO sync_work_resources(resource,scope,key,generation,token,lease_until) VALUES(?1,?2,?3,?4,?5,?6)
            ON CONFLICT(resource) DO UPDATE SET scope=excluded.scope,key=excluded.key,generation=excluded.generation,token=excluded.token,lease_until=excluded.lease_until",params![resource,scope,key,generation,token,until])?;
        postpone_resource(&tx, &resource)?;
        tx.commit()?;
        Ok(Some(WorkLease {
            scope: scope.to_owned(),
            work: PlannedWork {
                key,
                resource,
                payload: serde_json::from_str(&payload)?,
                depends_on: serde_json::from_str(&dependencies)?,
            },
            generation,
            token,
            attempts: attempts + 1,
        }))
    }
    pub fn renew(&self, lease: &WorkLease, now: i64, lease_ms: i64) -> Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let renewed=tx.execute("UPDATE sync_work_resources SET lease_until=?6 WHERE resource=?1 AND scope=?2 AND key=?3 AND generation=?4 AND token=?5",params![lease.work.resource,lease.scope,lease.work.key,lease.generation,lease.token,now.saturating_add(lease_ms)])?==1;
        if renewed {
            tx.execute("UPDATE sync_work SET lease_until=?5 WHERE scope=?1 AND key=?2 AND generation=?3 AND token=?4 AND state='running'",params![lease.scope,lease.work.key,lease.generation,lease.token,now.saturating_add(lease_ms)])?;
            postpone_resource(&tx, &lease.work.resource)?;
        }
        tx.commit()?;
        Ok(renewed)
    }
    pub fn finish(&self, lease: &WorkLease, result: WorkResult, now: i64) -> Result<bool> {
        let (state, error, delay) = match result {
            WorkResult::Complete => ("complete", None, 0),
            WorkResult::Conflict(error) => ("conflict", Some(error), 0),
            WorkResult::Retry(error) => (
                "pending",
                Some(error),
                1000_i64.saturating_mul(1_i64 << lease.attempts.min(8)),
            ),
        };
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied=tx.execute("UPDATE sync_work SET state=?5,error=?6,available_at=?7,lease_until=0,finished_at=?8 WHERE scope=?1 AND key=?2 AND generation=?3 AND token=?4 AND state='running'",params![lease.scope,lease.work.key,lease.generation,lease.token,state,error,now.saturating_add(delay),if state=="complete" {now}else{0}])?==1;
        tx.execute("DELETE FROM sync_work_resources WHERE resource=?1 AND scope=?2 AND key=?3 AND generation=?4 AND token=?5",params![lease.work.resource,lease.scope,lease.work.key,lease.generation,lease.token])?;
        refresh_ready(&tx, &lease.scope, &lease.work.key)?;
        postpone_resource(&tx, &lease.work.resource)?;
        if applied {
            refresh_dependents(&tx, &lease.scope, &lease.work.key)?;
        }
        tx.commit()?;
        Ok(applied)
    }
    pub fn execute(
        &self,
        lease: &WorkLease,
        execute: impl FnOnce() -> WorkResult,
    ) -> Result<(WorkResult, bool)> {
        let (stop, receiver) = std::sync::mpsc::channel();
        let store = self.clone();
        let owned = lease.clone();
        let heartbeat = std::thread::spawn(move || {
            while receiver
                .recv_timeout(Duration::from_secs(10))
                .is_err_and(|error| error == std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                match store.renew(&owned, now_ms(), 120_000) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => tracing::warn!("sync work lease renewal will retry: {error:#}"),
                }
            }
        });
        let result = execute();
        let _ = stop.send(());
        let _ = heartbeat.join();
        let applied = self.finish(lease, result.clone(), now_ms())?;
        Ok((result, applied))
    }
    pub fn rearm_completed(&self, scope: &str, key: &str) -> Result<()> {
        self.change_state(scope, key, "complete", "pending")
    }
    pub fn cancel(&self, scope: &str, key: &str) -> Result<()> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE sync_work SET state='cancelled',token=token+1,finished_at=?3 WHERE scope=?1 AND key=?2 AND state IN ('pending','running','conflict')",params![scope,key,now_ms()])?;
        refresh_ready(&tx, scope, key)?;
        refresh_dependents(&tx, scope, key)?;
        tx.commit()?;
        Ok(())
    }
    fn change_state(&self, scope: &str, key: &str, from: &str, to: &str) -> Result<()> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE sync_work SET state=?4,generation=generation+1,token=token+1,attempts=0,available_at=0,finished_at=0 WHERE scope=?1 AND key=?2 AND state=?3",params![scope,key,from,to])?;
        refresh_ready(&tx, scope, key)?;
        refresh_dependents(&tx, scope, key)?;
        tx.commit()?;
        Ok(())
    }
    /// Clear a conflict only after the adapter proves the desired state already
    /// exists or has an explicit resolution. The payload check rejects stale proof.
    pub fn complete_conflict(&self, scope: &str, work: &PlannedWork) -> Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied=tx.execute("UPDATE sync_work SET state='complete',token=token+1,finished_at=?4,error=NULL WHERE scope=?1 AND key=?2 AND state='conflict' AND payload=?3",params![scope,work.key,serde_json::to_string(&work.payload)?,now_ms()])?==1;
        if applied {
            refresh_dependents(&tx, scope, &work.key)?;
        }
        tx.commit()?;
        Ok(applied)
    }
    pub fn next_ready_at(&self, scope: &str) -> Result<Option<i64>> {
        Ok(self.lock()?.query_row(
            "SELECT min(available_at) FROM sync_work_ready WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )?)
    }
    /// Keep desired-state completions and dependency prerequisites. Historical
    /// terminal rows outside the current plan are retained only for the horizon.
    pub fn prune(&self, now: i64, retention_ms: i64) -> Result<usize> {
        if retention_ms < 0 {
            bail!("work retention must not be negative");
        }
        Ok(self.lock()?.execute("DELETE FROM sync_work WHERE state IN ('complete','cancelled') AND finished_at<?1
            AND NOT EXISTS(SELECT 1 FROM sync_work_desired d WHERE d.scope=sync_work.scope AND d.key=sync_work.key)
            AND NOT EXISTS(SELECT 1 FROM sync_work_resources r WHERE r.scope=sync_work.scope AND r.key=sync_work.key AND r.lease_until>?2)
            AND NOT EXISTS(SELECT 1 FROM sync_work_dependencies d JOIN sync_work child ON child.scope=d.scope AND child.key=d.key WHERE d.scope=sync_work.scope AND d.dependency=sync_work.key AND child.state IN ('pending','running','conflict'))",params![now.saturating_sub(retention_ms),now])?)
    }
    pub fn status(&self, scope: &str) -> Result<WorkStatus> {
        let connection = self.lock()?;
        let mut result = WorkStatus::default();
        let mut query = connection
            .prepare("SELECT state,count(*) FROM sync_work WHERE scope=?1 GROUP BY state")?;
        for row in query.query_map([scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })? {
            let (state, count) = row?;
            match state.as_str() {
                "pending" => result.pending = count,
                "running" => result.running = count,
                "conflict" => result.conflicts = count,
                "complete" => result.completed = count,
                _ => {}
            }
        }
        Ok(result)
    }
    pub fn work(&self, scope: &str) -> Result<Vec<PlannedWork>> {
        self.read_work(scope, "active")
    }
    pub fn all_work(&self, scope: &str) -> Result<Vec<PlannedWork>> {
        self.read_work(scope, "all")
    }
    pub fn conflicts(&self, scope: &str) -> Result<Vec<PlannedWork>> {
        self.read_work(scope, "conflict")
    }
    fn read_work(&self, scope: &str, selection: &str) -> Result<Vec<PlannedWork>> {
        let connection = self.lock()?;
        let mut query=connection.prepare("SELECT key,resource,payload,dependencies FROM sync_work WHERE scope=?1 AND ((?2='active' AND state IN ('pending','running','conflict')) OR (?2='all' AND state<>'cancelled') OR (?2='conflict' AND state='conflict')) ORDER BY key")?;
        query
            .query_map(params![scope, selection], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (key, resource, payload, dependencies) = row?;
                Ok(PlannedWork {
                    key,
                    resource,
                    payload: serde_json::from_str(&payload)?,
                    depends_on: serde_json::from_str(&dependencies)?,
                })
            })
            .collect()
    }
    pub fn observation<T: serde::de::DeserializeOwned>(&self, scope: &str) -> Result<Option<T>> {
        let payload: Option<String> = self
            .lock()?
            .query_row(
                "SELECT payload FROM sync_observations WHERE scope=?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }
    pub fn observe<T: Serialize>(&self, scope: &str, value: &T) -> Result<()> {
        self.lock()?.execute("INSERT INTO sync_observations(scope,payload) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET payload=excluded.payload WHERE payload<>excluded.payload",params![scope,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn path_observations<T: serde::de::DeserializeOwned>(
        &self,
        scope: &str,
    ) -> Result<std::collections::HashMap<String, T>> {
        let connection = self.lock()?;
        let mut query =
            connection.prepare("SELECT path,payload FROM sync_path_observations WHERE scope=?1")?;
        query
            .query_map([scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (path, payload) = row?;
                Ok((path, serde_json::from_str(&payload)?))
            })
            .collect()
    }
    pub fn observe_paths<T: Serialize>(
        &self,
        scope: &str,
        changed: &[(String, T)],
        removed: &[String],
    ) -> Result<()> {
        if changed.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (path, value) in changed {
            tx.execute("INSERT INTO sync_path_observations(scope,path,payload) VALUES(?1,?2,?3) ON CONFLICT(scope,path) DO UPDATE SET payload=excluded.payload WHERE payload<>excluded.payload",params![scope,path,serde_json::to_string(value)?])?;
        }
        for path in removed {
            tx.execute(
                "DELETE FROM sync_path_observations WHERE scope=?1 AND path=?2",
                params![scope, path],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))
    }
    #[cfg(test)]
    pub(super) fn claim_query_plan(&self) -> Result<Vec<String>> {
        Ok(self
            .lock()?
            .prepare(&format!("EXPLAIN QUERY PLAN {CLAIM_SQL}"))?
            .query_map(params!["test", 0, None::<&str>], |row| {
                row.get::<_, String>(3)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    #[cfg(test)]
    pub(super) fn ready_count(&self, scope: &str) -> Result<u64> {
        Ok(self.lock()?.query_row(
            "SELECT count(*) FROM sync_work_ready WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )?)
    }
}

fn refresh_ready(connection: &Connection, scope: &str, key: &str) -> Result<()> {
    connection.execute(
        "DELETE FROM sync_work_ready WHERE scope=?1 AND key=?2",
        params![scope, key],
    )?;
    connection.execute("INSERT INTO sync_work_ready(scope,key,available_at)
        SELECT w.scope,w.key,CASE WHEN w.state='running' THEN max(w.lease_until,coalesce(a.lease_until,0)) ELSE max(w.available_at,coalesce(a.lease_until,0)) END
        FROM sync_work w LEFT JOIN sync_work_resources a ON a.resource=w.resource
        WHERE w.scope=?1 AND w.key=?2 AND w.state IN ('pending','running') AND NOT EXISTS(
            SELECT 1 FROM sync_work_dependencies d LEFT JOIN sync_work p ON p.scope=d.scope AND p.key=d.dependency
            WHERE d.scope=w.scope AND d.key=w.key AND (p.state IS NULL OR p.state<>'complete'))",params![scope,key])?;
    Ok(())
}
fn refresh_dependents(connection: &Connection, scope: &str, key: &str) -> Result<()> {
    let children = connection
        .prepare("SELECT key FROM sync_work_dependencies WHERE scope=?1 AND dependency=?2")?
        .query_map(params![scope, key], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for child in children {
        refresh_ready(connection, scope, &child)?;
    }
    Ok(())
}
fn postpone_resource(connection: &Connection, resource: &str) -> Result<()> {
    connection.execute("UPDATE sync_work_ready SET available_at=(SELECT CASE WHEN w.state='running' THEN max(w.lease_until,coalesce(a.lease_until,0)) ELSE max(w.available_at,coalesce(a.lease_until,0)) END
        FROM sync_work w LEFT JOIN sync_work_resources a ON a.resource=w.resource WHERE w.scope=sync_work_ready.scope AND w.key=sync_work_ready.key)
        WHERE (scope,key) IN (SELECT scope,key FROM sync_work WHERE resource=?1 AND state IN ('pending','running'))",[resource])?;
    Ok(())
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
