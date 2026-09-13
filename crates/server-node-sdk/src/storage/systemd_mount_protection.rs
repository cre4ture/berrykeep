use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::media_tools::{
    HostDependencyCheck, HostDependencySeverity, HostDependencyStatus, resolve_host_dependency_path,
};
use super::{StoragePathConfig, StoragePathState};

const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemdMountProtectionInspection {
    NotManagedBySystemd,
    SystemctlMissing {
        service: String,
    },
    QueryFailed {
        service: String,
        reason: String,
    },
    Dependencies {
        service: String,
        mounts: Vec<SystemdMountDependency>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemdMountDependency {
    unit: String,
    where_path: PathBuf,
}

#[derive(Debug, Clone)]
struct MountProtectionTarget {
    id: String,
    feature: String,
    path: PathBuf,
    missing_severity: HostDependencySeverity,
}

pub(super) async fn mount_protection_checks(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<HostDependencyCheck> {
    let inspection = inspect_current_process().await;
    let targets = mount_protection_targets(data_dir, storage_paths);
    checks_for_inspection(&targets, inspection)
}

async fn inspect_current_process() -> SystemdMountProtectionInspection {
    let Some(service) = current_systemd_service() else {
        return SystemdMountProtectionInspection::NotManagedBySystemd;
    };

    let Some(systemctl) = resolve_host_dependency_path(Path::new("systemctl")) else {
        return SystemdMountProtectionInspection::SystemctlMissing { service };
    };

    let service_dependencies = match run_systemctl(
        &systemctl,
        [
            "show",
            "--all",
            "--property=Requires",
            "--property=BindsTo",
            service.as_str(),
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(reason) => return SystemdMountProtectionInspection::QueryFailed { service, reason },
    };

    let mount_units = direct_mount_units_from_service_properties(&service_dependencies);
    if mount_units.is_empty() {
        return SystemdMountProtectionInspection::Dependencies {
            service,
            mounts: Vec::new(),
        };
    }
    let mut where_arguments = Vec::with_capacity(mount_units.len() + 5);
    where_arguments.extend(["show", "--all", "--property=Where", "--value"]);
    where_arguments.extend(mount_units.iter().map(String::as_str));
    let where_output = match run_systemctl(&systemctl, where_arguments).await {
        Ok(output) => output,
        Err(reason) => return SystemdMountProtectionInspection::QueryFailed { service, reason },
    };
    let mounts = mount_dependencies_from_where_values(&mount_units, &where_output);

    SystemdMountProtectionInspection::Dependencies { service, mounts }
}

fn current_systemd_service() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let cgroups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        systemd_service_from_cgroups(&cgroups)
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn systemd_service_from_cgroups(cgroups: &str) -> Option<String> {
    cgroups
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, path)| path))
        .find_map(systemd_service_from_cgroup_path)
}

fn systemd_service_from_cgroup_path(path: &str) -> Option<String> {
    for unit in path.rsplit('/').filter(|component| !component.is_empty()) {
        // A scope contains processes started outside a service unit (such as a
        // login session). Do not mistake its user manager ancestor for the
        // BerryKeep server service.
        if unit.ends_with(".scope") {
            return None;
        }
        if is_systemd_service_unit(unit) && !is_systemd_user_manager_service(unit) {
            return Some(unit.to_string());
        }
    }
    None
}

fn is_systemd_service_unit(unit: &str) -> bool {
    unit.strip_suffix(".service")
        .is_some_and(|name| !name.is_empty())
}

fn is_systemd_user_manager_service(unit: &str) -> bool {
    unit.strip_suffix(".service")
        .is_some_and(|name| name.starts_with("user@"))
}

async fn run_systemctl<'a>(
    systemctl: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    let output = timeout(SYSTEMCTL_TIMEOUT, async {
        let mut command = Command::new(systemctl);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command.output().await
    })
    .await
    .map_err(|_| "systemctl did not respond within five seconds".to_string())?
    .map_err(|error| format!("failed to start systemctl: {error}"))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        Err(format!("systemctl exited with {}", output.status))
    } else {
        Err(format!("systemctl exited with {}: {detail}", output.status))
    }
}

fn direct_mount_units_from_service_properties(output: &str) -> Vec<String> {
    let mut units = BTreeSet::new();
    for (_, dependencies) in output
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(property, _)| matches!(*property, "Requires" | "BindsTo"))
    {
        units.extend(
            dependencies
                .split_whitespace()
                .filter_map(mount_unit_from_token),
        );
    }
    units.into_iter().collect()
}

fn mount_unit_from_token(token: &str) -> Option<String> {
    token.ends_with(".mount").then(|| token.to_string())
}

fn mount_dependencies_from_where_values(
    mount_units: &[String],
    where_values: &str,
) -> Vec<SystemdMountDependency> {
    mount_units
        .iter()
        .zip(where_values.split('\n'))
        .filter_map(|(unit, where_value)| {
            let where_value = where_value.trim();
            (!where_value.is_empty() && where_value != "-").then(|| SystemdMountDependency {
                unit: unit.clone(),
                where_path: PathBuf::from(where_value),
            })
        })
        .collect()
}

fn mount_protection_targets(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<MountProtectionTarget> {
    let mut targets = vec![MountProtectionTarget {
        id: "systemd-mount-data-dir".to_string(),
        feature: "Systemd mount protection: IRONMESH_DATA_DIR".to_string(),
        path: data_dir.to_path_buf(),
        missing_severity: HostDependencySeverity::Critical,
    }];

    targets.extend(
        storage_paths
            .iter()
            .filter(|path| !matches!(path.state, StoragePathState::Disabled))
            .map(|path| MountProtectionTarget {
                id: format!("systemd-mount-storage-{}", path.id),
                feature: format!(
                    "Systemd mount protection: storage pool `{}` ({})",
                    path.id,
                    storage_path_state_label(path.state)
                ),
                path: path.path.clone(),
                missing_severity: match path.state {
                    StoragePathState::Active => HostDependencySeverity::Critical,
                    StoragePathState::Draining => HostDependencySeverity::Warning,
                    StoragePathState::Disabled => HostDependencySeverity::Info,
                },
            }),
    );
    targets
}

fn storage_path_state_label(state: StoragePathState) -> &'static str {
    match state {
        StoragePathState::Active => "active",
        StoragePathState::Draining => "draining",
        StoragePathState::Disabled => "disabled",
    }
}

fn checks_for_inspection(
    targets: &[MountProtectionTarget],
    inspection: SystemdMountProtectionInspection,
) -> Vec<HostDependencyCheck> {
    match inspection {
        SystemdMountProtectionInspection::NotManagedBySystemd => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::NotApplicable,
                severity: HostDependencySeverity::Info,
                summary: "This server process is not managed by a systemd service".to_string(),
                detail: "Mount protection is checked only for a BerryKeep server node running inside a systemd service cgroup. The availability of Linux or systemctl alone does not make this check applicable.".to_string(),
                configured_path: None,
                resolved_path: None,
                install_hint: None,
            }]
        }
        SystemdMountProtectionInspection::SystemctlMissing { service } => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::Missing,
                severity: HostDependencySeverity::Info,
                summary: format!(
                    "The running service `{service}` was detected, but systemctl is unavailable"
                ),
                detail: "The server is managed by systemd, but the effective service dependency graph cannot be inspected without systemctl.".to_string(),
                configured_path: Some("systemctl".to_string()),
                resolved_path: None,
                install_hint: Some("Install or restore the systemd client tools that provide `systemctl`, then refresh this report.".to_string()),
            }]
        }
        SystemdMountProtectionInspection::QueryFailed { service, reason } => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::Missing,
                severity: HostDependencySeverity::Warning,
                summary: format!(
                    "Could not inspect effective systemd dependencies for `{service}`"
                ),
                detail: format!(
                    "The live systemd dependency graph could not be read: {reason}. Mount protection has not been verified."
                ),
                configured_path: Some(service.clone()),
                resolved_path: None,
                install_hint: Some(format!(
                    "Confirm that the server process may query `{service}` with `systemctl`, then refresh this report."
                )),
            }]
        }
        SystemdMountProtectionInspection::Dependencies { service, mounts } => targets
            .iter()
            .map(|target| {
                let protecting_mount = protecting_mount_dependency(&target.path, &mounts);
                match protecting_mount {
                    Some(mount) => HostDependencyCheck {
                        id: target.id.clone(),
                        feature: target.feature.clone(),
                        status: HostDependencyStatus::Ready,
                        severity: HostDependencySeverity::Info,
                        summary: format!(
                            "Effective dependencies for `{service}` include `{}` for {}",
                            mount.unit,
                            mount.where_path.display()
                        ),
                        detail: "The live systemd dependency properties are used, so direct unit dependencies, drop-ins, and systemd-created implicit mount dependencies are all included without accepting unrelated transitive mounts.".to_string(),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: Some(format!(
                            "{} ({})",
                            mount.unit,
                            mount.where_path.display()
                        )),
                        install_hint: None,
                    },
                    None => HostDependencyCheck {
                        id: target.id.clone(),
                        feature: target.feature.clone(),
                        status: HostDependencyStatus::Missing,
                        severity: target.missing_severity,
                        summary: format!(
                            "No effective systemd mount dependency protects {}",
                            target.path.display()
                        ),
                        detail: format!(
                            "If the filesystem containing this path is unavailable at boot or is later unmounted, `{service}` can start or continue without systemd tying its lifetime to that mount."
                        ),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: None,
                        install_hint: Some(format!(
                            "Add `RequiresMountsFor={}` to the [Unit] section of a drop-in for `{service}`, then run `sudo systemctl daemon-reload` and restart the service.",
                            target.path.display()
                        )),
                    },
                }
            })
            .collect(),
    }
}

fn protecting_mount_dependency<'a>(
    target: &Path,
    mounts: &'a [SystemdMountDependency],
) -> Option<&'a SystemdMountDependency> {
    mounts
        .iter()
        .filter(|mount| mount.where_path != Path::new("/") && target.starts_with(&mount.where_path))
        .max_by_key(|mount| mount.where_path.components().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_path(id: &str, path: &str, state: StoragePathState) -> StoragePathConfig {
        StoragePathConfig {
            id: id.to_string(),
            path: PathBuf::from(path),
            state,
            weight: 1,
            reserve_bytes: 0,
        }
    }

    fn systemd_mount(unit: &str, where_path: &str) -> SystemdMountDependency {
        SystemdMountDependency {
            unit: unit.to_string(),
            where_path: PathBuf::from(where_path),
        }
    }

    #[test]
    fn detects_the_actual_systemd_service_from_the_process_cgroup() {
        let service =
            systemd_service_from_cgroups("0::/system.slice/berrykeep-server-node.service\n");

        assert_eq!(service.as_deref(), Some("berrykeep-server-node.service"));
    }

    #[test]
    fn detects_a_service_from_a_nested_service_cgroup() {
        let service =
            systemd_service_from_cgroups("0::/system.slice/berrykeep-server-node.service/worker\n");

        assert_eq!(service.as_deref(), Some("berrykeep-server-node.service"));
    }

    #[test]
    fn does_not_treat_a_non_service_systemd_scope_as_a_managed_server_service() {
        let service = systemd_service_from_cgroups(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/session-4.scope\n",
        );

        assert!(service.is_none());
    }

    #[test]
    fn non_systemd_startup_is_not_applicable_and_never_warns() {
        let targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[storage_path(
                "primary",
                "/mnt/primary",
                StoragePathState::Active,
            )],
        );
        let checks = checks_for_inspection(
            &targets,
            SystemdMountProtectionInspection::NotManagedBySystemd,
        );

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, HostDependencyStatus::NotApplicable);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
    }

    #[test]
    fn systemd_checks_a_single_active_storage_path() {
        let targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[storage_path(
                "primary",
                "/mnt/primary",
                StoragePathState::Active,
            )],
        );
        let checks = checks_for_inspection(
            &targets,
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![
                    systemd_mount("srv.mount", "/srv"),
                    systemd_mount("mnt-primary.mount", "/mnt/primary"),
                ],
            },
        );

        assert_eq!(checks.len(), 2);
        assert!(
            checks
                .iter()
                .all(|check| check.status == HostDependencyStatus::Ready)
        );
    }

    #[test]
    fn systemd_checks_data_dir_and_active_or_draining_storage_paths() {
        let targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[
                storage_path("primary", "/mnt/primary", StoragePathState::Active),
                storage_path("archive", "/mnt/archive", StoragePathState::Draining),
                storage_path("retired", "/mnt/retired", StoragePathState::Disabled),
            ],
        );
        let checks = checks_for_inspection(
            &targets,
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![
                    systemd_mount("srv.mount", "/srv"),
                    systemd_mount("mnt-primary.mount", "/mnt/primary"),
                ],
            },
        );

        assert_eq!(checks.len(), 3);
        assert!(checks.iter().any(|check| {
            check.id == "systemd-mount-data-dir" && check.status == HostDependencyStatus::Ready
        }));
        assert!(checks.iter().any(|check| {
            check.id == "systemd-mount-storage-primary"
                && check.status == HostDependencyStatus::Ready
        }));
        let draining = checks
            .iter()
            .find(|check| check.id == "systemd-mount-storage-archive")
            .unwrap();
        assert_eq!(draining.status, HostDependencyStatus::Missing);
        assert_eq!(draining.severity, HostDependencySeverity::Warning);
        assert!(
            draining
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("RequiresMountsFor=/mnt/archive")
        );
        assert!(
            !checks
                .iter()
                .any(|check| check.id == "systemd-mount-storage-retired")
        );
    }

    #[test]
    fn direct_service_dependencies_extract_effective_mount_units() {
        let units = direct_mount_units_from_service_properties(
            "Requires=mnt-primary.mount sysinit.target\nBindsTo=var-lib-berrykeep.mount\nWants=unrelated.mount\nAfter=unrelated.mount\n",
        );

        assert_eq!(units, vec!["mnt-primary.mount", "var-lib-berrykeep.mount"]);
    }

    #[test]
    fn where_values_are_matched_to_mount_units_in_one_systemctl_response() {
        let mounts = mount_dependencies_from_where_values(
            &[
                "mnt-primary.mount".to_string(),
                "mnt-archive.mount".to_string(),
                "missing.mount".to_string(),
            ],
            "/mnt/primary\n/mnt/archive\n-\n",
        );

        assert_eq!(
            mounts,
            vec![
                systemd_mount("mnt-primary.mount", "/mnt/primary"),
                systemd_mount("mnt-archive.mount", "/mnt/archive"),
            ]
        );
    }

    #[test]
    fn root_mount_does_not_protect_a_missing_storage_mount() {
        let targets = mount_protection_targets(
            Path::new("/var/lib/berrykeep"),
            &[storage_path(
                "primary",
                "/mnt/primary",
                StoragePathState::Active,
            )],
        );
        let checks = checks_for_inspection(
            &targets,
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![systemd_mount("-.mount", "/")],
            },
        );

        assert!(
            checks
                .iter()
                .all(|check| check.status == HostDependencyStatus::Missing)
        );
        assert!(
            checks
                .iter()
                .all(|check| check.severity == HostDependencySeverity::Critical)
        );
    }
}
