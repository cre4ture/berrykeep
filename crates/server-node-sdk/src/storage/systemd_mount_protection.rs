use std::collections::{BTreeMap, BTreeSet};
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
    let mounted_at = mount_points_for_targets(&targets);
    checks_for_inspection(&targets, &mounted_at, inspection)
}

async fn inspect_current_process() -> SystemdMountProtectionInspection {
    let Some(service) = current_systemd_service() else {
        return SystemdMountProtectionInspection::NotManagedBySystemd;
    };

    let Some(systemctl) = resolve_host_dependency_path(Path::new("systemctl")) else {
        return SystemdMountProtectionInspection::SystemctlMissing { service };
    };

    let dependency_listing = match run_systemctl(
        &systemctl,
        [
            "list-dependencies",
            "--all",
            "--plain",
            "--no-pager",
            service.as_str(),
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(reason) => return SystemdMountProtectionInspection::QueryFailed { service, reason },
    };

    let mount_units = mount_units_from_dependency_listing(&dependency_listing);
    let mut mounts = Vec::with_capacity(mount_units.len());
    for unit in mount_units {
        let where_output = match run_systemctl(
            &systemctl,
            [
                "show",
                "--all",
                "--property=Where",
                "--value",
                unit.as_str(),
            ],
        )
        .await
        {
            Ok(output) => output,
            Err(reason) => {
                return SystemdMountProtectionInspection::QueryFailed { service, reason };
            }
        };
        let Some(where_path) = where_output
            .lines()
            .map(str::trim)
            .find(|value| !value.is_empty() && *value != "-")
            .map(PathBuf::from)
        else {
            continue;
        };
        mounts.push(SystemdMountDependency { unit, where_path });
    }

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
        .filter_map(|path| path.rsplit('/').find(|component| !component.is_empty()))
        .find(|unit| is_systemd_service_unit(unit))
        .map(str::to_string)
}

fn is_systemd_service_unit(unit: &str) -> bool {
    unit.strip_suffix(".service")
        .is_some_and(|name| !name.is_empty())
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

fn mount_units_from_dependency_listing(output: &str) -> Vec<String> {
    output
        .lines()
        .flat_map(str::split_whitespace)
        .filter_map(mount_unit_from_token)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn mount_unit_from_token(token: &str) -> Option<String> {
    let unit_end = token.find(".mount")? + ".mount".len();
    let unit = token[..unit_end].trim_start_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '\\' | '_' | '-' | '.' | '@')
    });
    unit.ends_with(".mount").then(|| unit.to_string())
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
    mounted_at: &BTreeMap<PathBuf, PathBuf>,
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
                let protecting_mount = protecting_mount_dependency(
                    &target.path,
                    mounted_at.get(&target.path).map(PathBuf::as_path),
                    &mounts,
                );
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
                        detail: "The live systemd dependency graph is used, so configured unit dependencies, drop-ins, and systemd-created implicit mount dependencies are all included.".to_string(),
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
    mounted_at: Option<&Path>,
    mounts: &'a [SystemdMountDependency],
) -> Option<&'a SystemdMountDependency> {
    let expected_mount = mounted_at.unwrap_or(target);
    mounts
        .iter()
        .filter(|mount| mount.where_path == expected_mount)
        .max_by_key(|mount| mount.where_path.components().count())
        .or_else(|| {
            mounted_at
                .is_none()
                .then(|| {
                    mounts
                        .iter()
                        .filter(|mount| {
                            mount.where_path != Path::new("/")
                                && target.starts_with(&mount.where_path)
                        })
                        .max_by_key(|mount| mount.where_path.components().count())
                })
                .flatten()
        })
}

fn mount_points_for_targets(targets: &[MountProtectionTarget]) -> BTreeMap<PathBuf, PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let mount_points = std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .map(|contents| mount_points_from_mountinfo(&contents))
            .unwrap_or_default();
        targets
            .iter()
            .filter_map(|target| {
                mount_point_for_path(&target.path, &mount_points)
                    .map(|mount_point| (target.path.clone(), mount_point))
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    {
        BTreeMap::new()
    }
}

fn mount_points_from_mountinfo(mountinfo: &str) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _mount_id = fields.next()?;
            let _parent_id = fields.next()?;
            let _major_minor = fields.next()?;
            let _root = fields.next()?;
            let mount_point = fields.next()?;
            Some(PathBuf::from(unescape_mountinfo_path(mount_point)))
        })
        .collect()
}

fn unescape_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn mount_point_for_path(path: &Path, mount_points: &[PathBuf]) -> Option<PathBuf> {
    mount_points
        .iter()
        .filter(|mount_point| path.starts_with(mount_point))
        .max_by_key(|mount_point| mount_point.components().count())
        .cloned()
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
            &BTreeMap::new(),
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
        let mounted_at = BTreeMap::from([
            (PathBuf::from("/srv/berrykeep"), PathBuf::from("/srv")),
            (PathBuf::from("/mnt/primary"), PathBuf::from("/mnt/primary")),
        ]);
        let checks = checks_for_inspection(
            &targets,
            &mounted_at,
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
        let mounted_at = BTreeMap::from([
            (PathBuf::from("/srv/berrykeep"), PathBuf::from("/srv")),
            (PathBuf::from("/mnt/primary"), PathBuf::from("/mnt/primary")),
            (PathBuf::from("/mnt/archive"), PathBuf::from("/mnt/archive")),
        ]);
        let checks = checks_for_inspection(
            &targets,
            &mounted_at,
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
    fn dependency_listing_extracts_effective_mount_units() {
        let units = mount_units_from_dependency_listing(
            "berrykeep-server-node.service\n● ├─mnt-primary.mount\n● └─var-lib-berrykeep.mount\n",
        );

        assert_eq!(units, vec!["mnt-primary.mount", "var-lib-berrykeep.mount"]);
    }
}
