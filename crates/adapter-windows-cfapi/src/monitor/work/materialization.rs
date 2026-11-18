use super::*;

impl CfapiWork {
    pub(crate) fn schedule(
        &self,
        plan: &CfapiActionPlan,
        inventory: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<()> {
        *self.inventory.lock().unwrap_or_else(|err| err.into_inner()) = inventory.clone();
        let protected_ids = self.protected_object_ids()?;
        let plan = CfapiActionPlan {
            actions: plan
                .actions
                .iter()
                .filter(|action| {
                    !self.suppressed_by_fence(action)
                        && !action_object_id(action).is_some_and(|id| protected_ids.contains(id))
                })
                .cloned()
                .collect(),
        };
        self.store.reconcile(
            MATERIALIZATION_SCOPE,
            &materialization_plan(&plan, &self.protected_paths()?)?,
        )?;
        self.notify();
        Ok(())
    }

    pub(crate) fn drain_materialization(&self) -> Result<()> {
        for _ in 0..8 {
            let Some(lease) = self
                .store
                .claim_next(MATERIALIZATION_SCOPE, now_ms(), 120_000)?
            else {
                break;
            };
            let work: Materialization = serde_json::from_value(lease.work.payload.clone())?;
            let (result, applied) =
                self.store
                    .execute(&lease, || match self.apply_materialization(&work) {
                        Ok(result) => result,
                        Err(error) => WorkResult::Retry(format!("{error:#}")),
                    })?;
            if !applied {
                tracing::debug!(
                    work = lease.work.key,
                    "CFAPI materialization superseded; newer work retained"
                );
                continue;
            }
            if !matches!(result, WorkResult::Complete) {
                tracing::warn!(
                    work = lease.work.key,
                    ?result,
                    "CFAPI materialization retained"
                );
            }
        }
        let status = self.store.status(MATERIALIZATION_SCOPE)?;
        if status.conflicts > 0 {
            tracing::warn!(conflicts=status.conflicts,journal=%self.store.path().display(),"CFAPI materialization conflicts require reconciliation");
        }
        Ok(())
    }

    pub(super) fn apply_materialization(&self, work: &Materialization) -> Result<WorkResult> {
        if work.availability != ContentAvailability::MetadataOnly {
            anyhow::bail!("CFAPI namespace executor requires metadata availability");
        }
        let gates = work
            .actions
            .iter()
            .map(action_path)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|path| crate::path_operations::gate(&self.root.join(path.replace('/', "\\"))))
            .collect::<Vec<_>>();
        let _path_operations = gates
            .iter()
            .map(|gate| gate.lock().unwrap_or_else(|e| e.into_inner()))
            .collect::<Vec<_>>();
        let protected = self.protected_paths()?;
        let mut conflict = None;
        let mut waiting = None;
        let actions = work
            .actions
            .iter()
            .filter(|action| {
                !is_protected(action_path(action), &protected) && !self.suppressed_by_fence(action)
            })
            .filter_map(|action| {
                let path = action_path(action);
                let full = self.root.join(path.replace('/', "\\"));
                if !full.exists()
                    && self
                        .baseline
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .contains_key(path)
                {
                    waiting = Some(format!(
                        "awaiting complete local topology reconciliation for missing {path}"
                    ));
                    return None;
                }
                if full.exists()
                    && !matches!(
                        action,
                        CfapiAction::EnsureDirectory {
                            object_id: None,
                            ..
                        }
                    )
                {
                    let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
                    if let Some(error) = entry.placeholder_inspection_error.as_ref() {
                        waiting = Some(error.clone());
                        return None;
                    }
                    if !entry.is_placeholder
                        || entry.placeholder_object_id.as_deref() != action_object_id(action)
                    {
                        conflict = Some(format!("local object identity differs at {path}"));
                        return None;
                    }
                    if entry
                        .placeholder_state
                        .is_some_and(placeholder_has_uploadable_local_content)
                    {
                        // Metadata already represents this revision; modified local bytes
                        // remain for the independent guarded upload path.
                        if entry.placeholder_revision.as_deref() != action_revision(action) {
                            conflict = Some(format!("locally modified placeholder at {path}"));
                        }
                        return None;
                    }
                }
                Some(action.clone())
            })
            .collect::<Vec<_>>();
        let mut inventory = self.inventory.lock().unwrap_or_else(|err| err.into_inner());
        let plan = CfapiActionPlan {
            actions: actions.clone(),
        };
        if crate::runtime::apply_action_plan_with_observed_placeholder_paths(
            &self.root,
            &plan,
            self.provider,
            true,
            Some(&inventory),
        )
        .is_err()
        {
            // A locked first entry must not prevent the remainder of its batch
            // from materializing. Successful entries are recognized on replay.
            for action in &actions {
                let _ = crate::runtime::apply_action_plan_with_observed_placeholder_paths(
                    &self.root,
                    &CfapiActionPlan {
                        actions: vec![action.clone()],
                    },
                    self.provider,
                    true,
                    Some(&inventory),
                );
            }
        }
        let mut observed = Vec::new();
        let mut retry = waiting;
        for action in &actions {
            let path = action_path(action);
            let full = self.root.join(path.replace('/', "\\"));
            if !full.exists() {
                retry = Some(format!("placeholder still absent at {path}"));
                continue;
            }
            let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
            if let Some(error) = entry.placeholder_inspection_error.as_ref() {
                retry = Some(error.clone());
                continue;
            }
            if action_object_id(action).is_some()
                && (entry.placeholder_object_id.as_deref() != action_object_id(action)
                    || entry.placeholder_revision.as_deref() != action_revision(action))
            {
                retry = Some(format!(
                    "placeholder identity/revision not applied at {path}"
                ));
                continue;
            }
            if let Some(object_id) = entry.placeholder_object_id.as_ref() {
                inventory
                    .entry(object_id.clone())
                    .or_default()
                    .insert(path.to_owned());
            }
            observed.push((path.to_owned(), PriorPath::from_observed(&entry)));
            self.tracker.record_plan(&CfapiActionPlan {
                actions: vec![action.clone()],
            });
            for parent in parent_directories_for_path(path) {
                let full = self.root.join(parent.replace('/', "\\"));
                if full.is_dir() {
                    observed.push((
                        parent.clone(),
                        PriorPath::from_observed(&snapshot_entry(&self.root, &parent, &full, true)),
                    ));
                }
            }
        }
        self.store.observe_paths(BASELINE_SCOPE, &observed, &[])?;
        self.baseline
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend(observed);
        Ok(match (retry, conflict) {
            (Some(error), _) => WorkResult::Retry(error),
            (None, Some(error)) => WorkResult::Conflict(error),
            _ => WorkResult::Complete,
        })
    }
}

pub(super) fn materialization_plan(
    plan: &CfapiActionPlan,
    protected: &BTreeSet<String>,
) -> Result<Vec<PlannedWork>> {
    let mut directories = BTreeMap::new();
    let mut files: BTreeMap<String, Vec<CfapiAction>> = BTreeMap::new();
    for action in &plan.actions {
        let path = action_path(action);
        if is_protected(path, protected)
            || matches!(action, CfapiAction::QueueUploadOnClose { .. })
            || matches!(
                action,
                CfapiAction::EnsureDirectory {
                    object_id: None,
                    ..
                }
            )
        {
            continue;
        }
        for parent in parent_directories_for_path(path) {
            directories
                .entry(parent.clone())
                .or_insert(CfapiAction::EnsureDirectory {
                    object_id: None,
                    path: parent,
                    remote_version: None,
                });
        }
        match action {
            CfapiAction::EnsureDirectory { .. } => {
                directories.insert(path.to_owned(), action.clone());
            }
            _ => files
                .entry(
                    path.rsplit_once('/')
                        .map_or("", |(parent, _)| parent)
                        .to_owned(),
                )
                .or_default()
                .push(action.clone()),
        }
    }
    let mut jobs = Vec::new();
    for (path, action) in &directories {
        let mut job = PlannedWork::new(
            format!("directory:{path}"),
            format!("path:{path}"),
            serde_json::to_value(Materialization {
                actions: vec![action.clone()],
                availability: ContentAvailability::MetadataOnly,
            })?,
        );
        job.depends_on = parent_directories_for_path(path)
            .into_iter()
            .filter(|parent| directories.contains_key(parent))
            .map(|parent| format!("directory:{parent}"))
            .collect();
        jobs.push(job);
    }
    for (parent, mut actions) in files {
        actions.sort_by(|left, right| action_path(left).cmp(action_path(right)));
        for (index, chunk) in actions.chunks(64).enumerate() {
            let mut job = PlannedWork::new(
                format!("files:{parent}:{index:08}"),
                format!("directory-children:{parent}"),
                serde_json::to_value(Materialization {
                    actions: chunk.to_vec(),
                    availability: ContentAvailability::MetadataOnly,
                })?,
            );
            if !parent.is_empty() {
                job.depends_on.push(format!("directory:{parent}"));
            }
            jobs.push(job);
        }
    }
    Ok(jobs)
}
