//! Durable, adapter-independent operation lifecycle. Filesystem effects are deliberately
//! outside SQLite transactions; executors must inspect actual state when replaying work.

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

/// Availability is independent of namespace identity and of locally modified bytes.
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

/// Clones share a connection. Separately opened stores coordinate through SQLite.
#[derive(Clone)]
pub struct WorkStore {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl WorkStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening sync work journal {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS sync_work (
                scope TEXT NOT NULL, key TEXT NOT NULL, resource TEXT NOT NULL,
                payload TEXT NOT NULL, dependencies TEXT NOT NULL,
                generation INTEGER NOT NULL DEFAULT 1, token INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                available_at INTEGER NOT NULL DEFAULT 0, lease_until INTEGER NOT NULL DEFAULT 0,
                error TEXT, PRIMARY KEY(scope,key));
            CREATE INDEX IF NOT EXISTS sync_work_ready ON sync_work(scope,state,available_at);
            CREATE TABLE IF NOT EXISTS sync_work_resources (
                resource TEXT PRIMARY KEY, scope TEXT NOT NULL, key TEXT NOT NULL,
                generation INTEGER NOT NULL, token INTEGER NOT NULL, lease_until INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS sync_observations (
                scope TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sync_path_observations (
                scope TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(scope,path));",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Add durable intent. Existing identical work retains its retry/completion state.
    pub fn enqueue(&self, scope: &str, work: &[PlannedWork]) -> Result<()> {
        self.write_plan(scope, work, false)
    }

    /// Replace desired work, cancelling removed and superseded generations. Call only
    /// with a complete authoritative plan, never with a partial directory scan.
    pub fn reconcile(&self, scope: &str, work: &[PlannedWork]) -> Result<()> {
        self.write_plan(scope, work, true)
    }

    fn write_plan(&self, scope: &str, work: &[PlannedWork], replace: bool) -> Result<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if replace {
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS desired_work_keys(key TEXT PRIMARY KEY); DELETE FROM desired_work_keys;")?;
        }
        for item in work {
            if item.key.is_empty() || item.resource.is_empty() {
                bail!("sync work requires a key and resource");
            }
            let payload = serde_json::to_string(&item.payload)?;
            let dependencies = serde_json::to_string(&item.depends_on)?;
            tx.execute("INSERT INTO sync_work(scope,key,resource,payload,dependencies) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(scope,key) DO UPDATE SET resource=excluded.resource,payload=excluded.payload,
                dependencies=excluded.dependencies,generation=sync_work.generation+1,
                token=sync_work.token+1,state='pending',attempts=0,available_at=0,lease_until=0,error=NULL
                WHERE sync_work.payload<>excluded.payload OR sync_work.dependencies<>excluded.dependencies
                   OR sync_work.resource<>excluded.resource OR sync_work.state='cancelled'",
                params![scope,item.key,item.resource,payload,dependencies])?;
            if replace {
                tx.execute("INSERT INTO desired_work_keys(key) VALUES(?1)", [&item.key])?;
            }
        }
        if replace {
            tx.execute(
                "UPDATE sync_work SET state='cancelled',token=token+1 WHERE scope=?1
                AND state<>'cancelled' AND key NOT IN (SELECT key FROM desired_work_keys)",
                [scope],
            )?;
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

    pub fn rearm_completed(&self, scope: &str, key: &str) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        connection.execute("UPDATE sync_work SET state='pending',generation=generation+1,token=token+1,attempts=0,available_at=0 WHERE scope=?1 AND key=?2 AND state='complete'",params![scope,key])?;
        Ok(())
    }

    pub fn cancel(&self, scope: &str, key: &str) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        connection.execute("UPDATE sync_work SET state='cancelled',token=token+1 WHERE scope=?1 AND key=?2 AND state IN ('pending','running','conflict')",params![scope,key])?;
        Ok(())
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
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = tx.query_row("SELECT key,resource,payload,dependencies,generation,token,attempts FROM sync_work w
            WHERE scope=?1 AND (?3 IS NULL OR key=?3) AND ((state='pending' AND available_at<=?2) OR (state='running' AND lease_until<=?2))
            AND NOT EXISTS(SELECT 1 FROM json_each(w.dependencies) d LEFT JOIN sync_work p ON p.scope=w.scope AND p.key=d.value
                WHERE p.state IS NULL OR p.state<>'complete')
            AND NOT EXISTS(SELECT 1 FROM sync_work_resources active WHERE active.resource=w.resource
                AND active.lease_until>?2)
            ORDER BY key LIMIT 1", params![scope,now,key], |row| {
                Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,u32>(6)?))
            }).optional()?;
        let Some((key, resource, payload, dependencies, generation, token, attempts)) = candidate
        else {
            return Ok(None);
        };
        let token = token + 1;
        tx.execute("UPDATE sync_work SET state='running',token=?3,attempts=attempts+1,lease_until=?4 WHERE scope=?1 AND key=?2",
            params![scope,key,token,now.saturating_add(lease_ms)])?;
        tx.execute("INSERT INTO sync_work_resources(resource,scope,key,generation,token,lease_until) VALUES(?1,?2,?3,?4,?5,?6)
            ON CONFLICT(resource) DO UPDATE SET scope=excluded.scope,key=excluded.key,generation=excluded.generation,token=excluded.token,lease_until=excluded.lease_until",
            params![resource,scope,key,generation,token,now.saturating_add(lease_ms)])?;
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let resource_renewed = connection.execute("UPDATE sync_work_resources SET lease_until=?6 WHERE resource=?1 AND scope=?2 AND key=?3 AND generation=?4 AND token=?5",
            params![lease.work.resource,lease.scope,lease.work.key,lease.generation,lease.token,now.saturating_add(lease_ms)])? == 1;
        if resource_renewed {
            connection.execute("UPDATE sync_work SET lease_until=?5 WHERE scope=?1 AND key=?2 AND generation=?3 AND token=?4 AND state='running'",
                params![lease.scope,lease.work.key,lease.generation,lease.token,now.saturating_add(lease_ms)])?;
        }
        Ok(resource_renewed)
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
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied = tx.execute(
            "UPDATE sync_work SET state=?5,error=?6,available_at=?7,lease_until=0
            WHERE scope=?1 AND key=?2 AND generation=?3 AND token=?4 AND state='running'",
            params![
                lease.scope,
                lease.work.key,
                lease.generation,
                lease.token,
                state,
                error,
                now.saturating_add(delay)
            ],
        )? == 1;
        tx.execute("DELETE FROM sync_work_resources WHERE resource=?1 AND scope=?2 AND key=?3 AND generation=?4 AND token=?5",
            params![lease.work.resource,lease.scope,lease.work.key,lease.generation,lease.token])?;
        tx.commit()?;
        Ok(applied)
    }

    /// Keep long downloads leased while their synchronous backend is working.
    /// Adapters serialize reconciliation and execution for the same local tree.
    pub fn execute(
        &self,
        lease: &WorkLease,
        execute: impl FnOnce() -> WorkResult,
    ) -> Result<(WorkResult, bool)> {
        let (stop, receiver) = std::sync::mpsc::channel();
        let store = self.clone();
        let owned_lease = lease.clone();
        let heartbeat = std::thread::spawn(move || {
            while receiver
                .recv_timeout(Duration::from_secs(10))
                .is_err_and(|err| err == std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                if !store
                    .renew(&owned_lease, now_ms(), 120_000)
                    .unwrap_or(false)
                {
                    break;
                }
            }
        });
        let result = execute();
        let _ = stop.send(());
        let _ = heartbeat.join();
        let applied = self.finish(lease, result.clone(), now_ms())?;
        Ok((result, applied))
    }

    pub fn status(&self, scope: &str) -> Result<WorkStatus> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let mut query = connection.prepare("SELECT key,resource,payload,dependencies FROM sync_work WHERE scope=?1 AND state IN ('pending','running','conflict') ORDER BY key")?;
        let rows = query.query_map([scope], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        let value: Option<String> = connection
            .query_row(
                "SELECT payload FROM sync_observations WHERE scope=?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    /// Persist observations only after derived intents have been durably enqueued.
    pub fn observe<T: Serialize>(&self, scope: &str, value: &T) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
        connection.execute("INSERT INTO sync_observations(scope,payload) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET payload=excluded.payload",params![scope,serde_json::to_string(value)?])?;
        Ok(())
    }

    pub fn path_observations<T: serde::de::DeserializeOwned>(
        &self,
        scope: &str,
    ) -> Result<std::collections::HashMap<String, T>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
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

    /// Persist only changed paths in one transaction, instead of rewriting a tree.
    pub fn observe_paths<T: Serialize>(
        &self,
        scope: &str,
        changed: &[(String, T)],
        removed: &[String],
    ) -> Result<()> {
        if changed.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sync work connection poisoned"))?;
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
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
