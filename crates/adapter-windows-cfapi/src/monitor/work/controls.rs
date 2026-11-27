use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictDescription {
    pub path: String,
    pub operation: String,
    pub reason: String,
}

impl ConflictDescription {
    pub(super) fn materialization(path: &str, reason: String) -> Self {
        Self {
            path: path.to_owned(),
            operation: "materialize".into(),
            reason,
        }
    }
}

impl CfapiWork {
    /// Control connections read persisted reasons without opening an executor
    /// or probing the whole sync root's placeholder inventory.
    pub(crate) fn conflicts_in(store: &WorkStore) -> Result<Vec<ConflictDescription>> {
        let mut entries = Vec::new();
        for scope in [
            UPLOAD_SCOPE,
            LOCAL_SCOPE,
            MATERIALIZATION_SCOPE,
            RECEIPT_SCOPE,
        ] {
            for conflict in store.conflict_details(scope)? {
                if scope == MATERIALIZATION_SCOPE {
                    if let Ok(details) =
                        serde_json::from_str::<Vec<ConflictDescription>>(&conflict.reason)
                    {
                        entries.extend(details);
                        continue;
                    }
                    let work: Materialization = serde_json::from_value(conflict.work.payload)?;
                    entries.extend(work.actions.iter().map(|action| {
                        ConflictDescription::materialization(
                            action_path(action),
                            conflict.reason.clone(),
                        )
                    }));
                    continue;
                }
                let (path, operation) = if scope == LOCAL_SCOPE {
                    match serde_json::from_value::<LocalMutation>(conflict.work.payload)? {
                        LocalMutation::Delete { path, .. } => (path, "delete"),
                        LocalMutation::Rename { to, .. } => (to, "rename"),
                    }
                } else {
                    (
                        conflict.work.key.trim_end_matches('/').to_owned(),
                        if scope == UPLOAD_SCOPE {
                            "upload"
                        } else {
                            "repair accepted upload"
                        },
                    )
                };
                entries.push(ConflictDescription {
                    path,
                    operation: operation.into(),
                    reason: conflict.reason,
                });
            }
        }
        entries.sort_by(|a, b| (&a.path, &a.operation).cmp(&(&b.path, &b.operation)));
        Ok(entries)
    }
}
