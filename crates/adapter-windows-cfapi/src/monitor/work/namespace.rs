use super::*;

impl CfapiWork {
    /// Persist the old namespace removal before allowing
    /// a kind change to advance the scan baseline. Called while the path gate is
    /// held; execution remains with the complete-topology namespace worker.
    pub(in crate::monitor) fn stage_kind_replacement(
        &self,
        path: &str,
        is_dir: bool,
    ) -> Result<bool> {
        let normalized = normalize_path(path);
        let path = normalized.trim_end_matches('/');
        let deletes = {
            let baseline = self.baseline.lock().unwrap_or_else(|e| e.into_inner());
            let Some(old) = baseline.get(path).filter(|old| {
                old.is_dir != is_dir && old.object_id.is_some() && old.revision.is_some()
            }) else {
                return Ok(false);
            };
            let prefix = format!("{path}/");
            baseline
                .iter()
                .filter(|(candidate, _)| {
                    candidate.as_str() == path || (old.is_dir && candidate.starts_with(&prefix))
                })
                .filter_map(|(path, old)| {
                    Some(LocalMutation::Delete {
                        path: path.clone(),
                        renamed_from: None,
                        object_id: old.object_id.clone()?,
                        revision: old.revision.clone()?,
                        is_dir: old.is_dir,
                    })
                })
                .collect::<Vec<_>>()
        };
        self.enqueue_deletes(&deletes)?;
        self.store.cancel(
            UPLOAD_SCOPE,
            &if is_dir {
                path.to_owned()
            } else {
                format!("{path}/")
            },
        )?;
        Ok(true)
    }
    pub(in crate::monitor) fn local_intents(&self) -> Result<Vec<LocalMutation>> {
        self.store
            .work(LOCAL_SCOPE)?
            .into_iter()
            .map(|work| serde_json::from_value(work.payload).map_err(Into::into))
            .collect()
    }

    pub(crate) fn protected_object_ids(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .local_intents()?
            .iter()
            .map(|intent| intent.object_id().to_owned())
            .collect())
    }

    pub(crate) fn protected_snapshot_ids(
        &self,
        snapshot: &sync_core::SyncSnapshot,
    ) -> Result<BTreeSet<String>> {
        let mut ids = self.protected_object_ids()?;
        let fences = self.fences.lock().unwrap_or_else(|err| err.into_inner());
        for entry in &snapshot.remote {
            if fence_lookup_key(entry.object_id.as_deref(), entry.version.as_deref())
                .and_then(|key| fences.get(&key))
                .is_some_and(|intent| {
                    fence_suppresses(
                        intent,
                        entry.object_id.as_deref(),
                        entry.version.as_deref(),
                        &entry.path,
                    )
                })
                && let Some(id) = entry.object_id.as_ref()
            {
                ids.insert(id.clone());
            }
        }
        Ok(ids)
    }

    pub(super) fn suppressed_by_fence(&self, action: &CfapiAction) -> bool {
        let fences = self.fences.lock().unwrap_or_else(|err| err.into_inner());
        fence_lookup_key(action_object_id(action), action_revision(action))
            .and_then(|key| fences.get(&key))
            .is_some_and(|intent| {
                fence_suppresses(
                    intent,
                    action_object_id(action),
                    action_revision(action),
                    action_path(action),
                )
            })
    }

    fn completed_mutation(&self, intent: &LocalMutation) -> WorkResult {
        let revision = match intent {
            LocalMutation::Delete { revision, .. } | LocalMutation::Rename { revision, .. } => {
                revision
            }
        };
        let key = fence_lookup_key(Some(intent.object_id()), Some(revision))
            .expect("complete mutation identity");
        let recorded = self
            .record_fence(
                key,
                RevisionFence {
                    object_id: intent.object_id().to_owned(),
                    revision: revision.to_owned(),
                },
            )
            .and_then(|()| {
                if let LocalMutation::Delete {
                    path,
                    object_id,
                    revision,
                    ..
                } = intent
                {
                    let mut baseline = self.baseline.lock().unwrap_or_else(|e| e.into_inner());
                    if baseline.get(path).is_some_and(|entry| {
                        entry.object_id.as_deref() == Some(object_id)
                            && entry.revision.as_deref() == Some(revision)
                    }) {
                        self.store.observe_paths::<PriorPath>(
                            BASELINE_SCOPE,
                            &[],
                            std::slice::from_ref(path),
                        )?;
                        baseline.remove(path);
                    }
                }
                Ok(())
            });
        match recorded {
            Ok(()) => WorkResult::Complete,
            Err(error) => {
                WorkResult::Retry(format!("persisting completed mutation state: {error:#}"))
            }
        }
    }

    pub(super) fn record_fence(&self, key: String, fence: RevisionFence) -> Result<()> {
        self.store
            .observe_paths(FENCE_SCOPE, &[(key.clone(), fence.clone())], &[])?;
        self.fences
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(key, fence);
        Ok(())
    }

    pub(in crate::monitor) fn protected_paths(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .local_intents()?
            .iter()
            .flat_map(|intent| intent.paths().into_iter().map(ToOwned::to_owned))
            .collect())
    }

    #[cfg(test)]
    pub(in crate::monitor) fn enqueue_intent(&self, intent: &LocalMutation) -> Result<()> {
        self.enqueue_intents(std::slice::from_ref(intent))
    }

    pub(in crate::monitor) fn enqueue_intents(&self, intents: &[LocalMutation]) -> Result<()> {
        let jobs = intents
            .iter()
            .map(|intent| {
                Ok(PlannedWork::new(
                    intent.object_id(),
                    intent.object_id(),
                    serde_json::to_value(intent)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        self.store.enqueue(LOCAL_SCOPE, &jobs)?;
        self.notify();
        Ok(())
    }

    pub(in crate::monitor) fn enqueue_deletes(&self, intents: &[LocalMutation]) -> Result<()> {
        let existing = self.store.work(LOCAL_SCOPE)?;
        let mut all = BTreeMap::new();
        for job in &existing {
            let intent: LocalMutation = serde_json::from_value(job.payload.clone())?;
            if matches!(intent, LocalMutation::Delete { .. }) {
                all.insert(intent.object_id().to_owned(), intent);
            }
        }
        for intent in intents {
            all.insert(intent.object_id().to_owned(), intent.clone());
        }
        let intents = all.into_values().collect::<Vec<_>>();
        let dependencies = existing
            .iter()
            .map(|job| (job.key.as_str(), &job.depends_on))
            .collect::<HashMap<_, _>>();
        let paths = intents
            .iter()
            .filter_map(|intent| match intent {
                LocalMutation::Delete {
                    path, object_id, ..
                } => Some((path.clone(), object_id)),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let mut jobs = Vec::new();
        for intent in &intents {
            let mut job = PlannedWork::new(
                intent.object_id(),
                intent.object_id(),
                serde_json::to_value(intent)?,
            );
            if let LocalMutation::Delete {
                path, is_dir: true, ..
            } = intent
            {
                let prefix = format!("{path}/");
                job.depends_on = dependencies
                    .get(job.key.as_str())
                    .map(|deps| (*deps).clone())
                    .unwrap_or_default();
                job.depends_on.extend(
                    paths
                        .range(prefix.clone()..)
                        .take_while(|(path, _)| path.starts_with(&prefix))
                        .map(|(_, id)| (*id).clone())
                        .collect::<Vec<_>>(),
                );
                job.depends_on.sort();
                job.depends_on.dedup();
            }
            jobs.push(job);
        }
        self.store.enqueue(LOCAL_SCOPE, &jobs)?;
        self.notify();
        Ok(())
    }

    /// A complete topology scan can revise a pending destination by stable ID,
    /// even while FileIdentity still contains the original source path.
    pub(in crate::monitor) fn reconcile_pending_renames(
        &self,
        current: &HashMap<String, ObservedEntry>,
    ) -> Result<()> {
        let mut deletes = Vec::new();
        let mut renames = Vec::new();
        let mut by_id: HashMap<&str, Vec<(&String, &ObservedEntry)>> = HashMap::new();
        let unknown_identity = current
            .values()
            .any(|entry| entry.placeholder_inspection_error.is_some());
        for (path, entry) in current {
            if let Some(id) = entry.placeholder_object_id.as_deref() {
                by_id.entry(id).or_default().push((path, entry));
            }
        }
        for intent in self.local_intents()? {
            let LocalMutation::Rename {
                from,
                to,
                object_id,
                revision,
                is_dir,
            } = intent
            else {
                continue;
            };
            match by_id
                .get(object_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                [(path, entry)] if **path != to && entry.is_dir == is_dir => {
                    renames.push(LocalMutation::Rename {
                        from,
                        to: (*path).clone(),
                        object_id,
                        revision,
                        is_dir,
                    });
                }
                [] if !current.contains_key(&to) && !unknown_identity => {
                    deletes.push(LocalMutation::Delete {
                        path: to,
                        renamed_from: Some(from),
                        object_id,
                        revision,
                        is_dir,
                    });
                }
                _ => {}
            }
        }
        self.enqueue_intents(&renames)?;
        self.enqueue_deletes(&deletes)
    }

    pub(in crate::monitor) fn drain_mutations(&self, uploader: &dyn Uploader) -> Result<()> {
        for _ in 0..64 {
            let Some(lease) = self.store.claim_next(LOCAL_SCOPE, now_ms(), 120_000)? else {
                break;
            };
            let intent: LocalMutation = serde_json::from_value(lease.work.payload.clone())?;
            let (result, applied) = self
                .store
                .execute(&lease, || self.apply_mutation(uploader, &intent))?;
            if !applied {
                tracing::debug!(
                    work = lease.work.key,
                    "CFAPI local intent superseded; newer work retained"
                );
                continue;
            }
            if matches!(result, WorkResult::Conflict(_))
                && let LocalMutation::Rename { to, .. } = &intent
            {
                mark_local_rename_conflict(&self.root.join(to.replace('/', "\\")));
            }
            if !matches!(result, WorkResult::Complete) {
                tracing::warn!(?intent,?result,journal=%self.store.path().display(),"CFAPI local mutation retained");
            }
        }
        Ok(())
    }

    fn apply_mutation(&self, uploader: &dyn Uploader, intent: &LocalMutation) -> WorkResult {
        use crate::placeholder_metadata::RemoteObjectResolution;
        let gates = crate::path_operations::gates(
            intent
                .paths()
                .into_iter()
                .map(|path| self.root.join(path.replace('/', "\\"))),
        );
        let _path_operations = gates
            .iter()
            .map(|gate| gate.lock().unwrap_or_else(|e| e.into_inner()))
            .collect::<Vec<_>>();
        match intent {
            LocalMutation::Delete {
                path,
                object_id,
                revision,
                ..
            } => {
                let full = self.root.join(path.replace('/', "\\"));
                if full.exists() {
                    let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
                    if entry.placeholder_inspection_error.is_some() {
                        return WorkResult::Retry(format!(
                            "delete target identity unavailable at {path}"
                        ));
                    }
                    if entry.placeholder_object_id.as_deref() == Some(object_id) {
                        return WorkResult::Conflict(format!(
                            "deleted object was restored locally at {path}"
                        ));
                    }
                }
                match uploader.delete_object(object_id, revision) {
                    Ok(()) => self.completed_mutation(intent),
                    Err(error) => match uploader.resolve_remote_object(object_id) {
                        Ok(RemoteObjectResolution::Tombstone(_)) => self.completed_mutation(intent),
                        Ok(RemoteObjectResolution::Present(remote))
                            if remote.revision.as_deref() != Some(revision) =>
                        {
                            WorkResult::Conflict(format!(
                                "delete expected {revision}, remote revision is {:?}",
                                remote.revision
                            ))
                        }
                        _ if is_remote_mutation_conflict(&error) => {
                            WorkResult::Conflict(format!("{error:#}"))
                        }
                        _ => WorkResult::Retry(format!("{error:#}")),
                    },
                }
            }
            LocalMutation::Rename {
                from,
                to,
                object_id,
                revision,
                is_dir,
            } => {
                let full = self.root.join(to.replace('/', "\\"));
                let entry = snapshot_entry(&self.root, to, &full, *is_dir);
                if entry.placeholder_inspection_error.is_some() {
                    return WorkResult::Retry(format!(
                        "rename destination identity unavailable at {to}"
                    ));
                }
                if entry.placeholder_object_id.as_deref() != Some(object_id) {
                    return WorkResult::Conflict(format!(
                        "rename destination identity changed at {to}"
                    ));
                }
                let receipt = match uploader.rename_object(
                    object_id,
                    revision,
                    &remote_rename_destination(to, *is_dir),
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => match uploader.resolve_remote_object(object_id) {
                        Ok(RemoteObjectResolution::Present(remote))
                            if normalize_path(&remote.path).trim_end_matches('/') == to
                                && remote
                                    .revision
                                    .as_ref()
                                    .is_some_and(|revision| !revision.is_empty()) =>
                        {
                            crate::runtime::ObjectRenameReceipt {
                                object_id: remote.object_id,
                                remote_version: remote.revision.unwrap(),
                            }
                        }
                        Ok(RemoteObjectResolution::Present(remote))
                            if normalize_path(&remote.path).trim_end_matches('/') != from
                                || remote.revision.as_deref() != Some(revision) =>
                        {
                            return WorkResult::Conflict(format!(
                                "rename object changed remotely: {remote:?}"
                            ));
                        }
                        _ if is_remote_mutation_conflict(&error) => {
                            return WorkResult::Conflict(format!("{error:#}"));
                        }
                        _ => return WorkResult::Retry(format!("{error:#}")),
                    },
                };
                if receipt.object_id != *object_id || receipt.remote_version.trim().is_empty() {
                    return WorkResult::Conflict("invalid rename receipt identity".into());
                }
                if let Err(error) = repair_locally_renamed_object(
                    &self.root,
                    &full,
                    to,
                    self.provider,
                    *is_dir,
                    &receipt,
                ) {
                    return WorkResult::Retry(format!("repairing rename receipt: {error:#}"));
                }
                let entry = PriorPath {
                    is_dir: *is_dir,
                    object_id: Some(object_id.clone()),
                    revision: Some(receipt.remote_version),
                };
                let mut baseline = self.baseline.lock().unwrap_or_else(|err| err.into_inner());
                if let Err(error) = self.store.observe_paths(
                    BASELINE_SCOPE,
                    &[(to.clone(), entry.clone())],
                    std::slice::from_ref(from),
                ) {
                    return WorkResult::Retry(format!("persisting rename receipt: {error:#}"));
                }
                baseline.remove(from);
                baseline.insert(to.clone(), entry);
                self.completed_mutation(intent)
            }
        }
    }
}
