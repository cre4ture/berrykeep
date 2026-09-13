#[cfg(any(target_os = "linux", test))]
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
#[cfg(any(target_os = "linux", test))]
use std::path::{Component, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use tokio::process::Command;
#[cfg(target_os = "linux")]
use tokio::time::timeout;

use super::StoragePathConfig;
#[cfg(any(target_os = "linux", test))]
use super::StoragePathState;
#[cfg(target_os = "linux")]
use super::media_tools::resolve_host_dependency_path;
use super::media_tools::{HostDependencyCheck, HostDependencySeverity, HostDependencyStatus};

#[cfg(target_os = "linux")]
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemdMountProtectionInspection {
    NotManagedBySystemd,
    #[cfg(target_os = "linux")]
    SystemctlMissing {
        service: String,
    },
    #[cfg(any(target_os = "linux", test))]
    QueryFailed {
        service: String,
        reason: String,
    },
    #[cfg(any(target_os = "linux", test))]
    UserManagedService {
        service: String,
    },
    #[cfg(any(target_os = "linux", test))]
    Dependencies {
        service: String,
        mounts: Vec<SystemdMountDependency>,
    },
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemdMountDependency {
    unit: String,
    where_path: PathBuf,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemdService {
    name: String,
    manager: SystemdServiceManager,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemdServiceManager {
    System,
    User,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
struct MountProtectionTarget {
    id: String,
    feature: String,
    path: PathBuf,
    mount_point: Option<PathBuf>,
    allows_root_filesystem: bool,
    missing_severity: HostDependencySeverity,
}

pub(super) async fn mount_protection_checks(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<HostDependencyCheck> {
    #[cfg(any(target_os = "linux", test))]
    {
        return mount_protection_checks_for_current_process(data_dir, storage_paths).await;
    }

    #[cfg(not(any(target_os = "linux", test)))]
    {
        let _ = (data_dir, storage_paths);
        vec![not_managed_by_systemd_check()]
    }
}

fn not_managed_by_systemd_check() -> HostDependencyCheck {
    HostDependencyCheck {
        id: "systemd-mount-protection".to_string(),
        feature: "Systemd mount protection".to_string(),
        status: HostDependencyStatus::NotApplicable,
        severity: HostDependencySeverity::Info,
        summary: "This server process is not managed by a systemd service".to_string(),
        detail: "Mount protection is checked only for a BerryKeep server node running inside a systemd service cgroup. The availability of Linux or systemctl alone does not make this check applicable.".to_string(),
        configured_path: None,
        resolved_path: None,
        install_hint: None,
    }
}

#[cfg(any(target_os = "linux", test))]
async fn mount_protection_checks_for_current_process(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<HostDependencyCheck> {
    let inspection = inspect_current_process().await;
    let targets = match &inspection {
        SystemdMountProtectionInspection::Dependencies { service, .. } => {
            match mount_protection_targets_for_current_process(data_dir, storage_paths).await {
                Ok(targets) => targets,
                Err(reason) => {
                    return checks_for_inspection(
                        &[],
                        SystemdMountProtectionInspection::QueryFailed {
                            service: service.clone(),
                            reason,
                        },
                    );
                }
            }
        }
        _ => Vec::new(),
    };
    checks_for_inspection(&targets, inspection)
}

#[cfg(target_os = "linux")]
async fn inspect_current_process() -> SystemdMountProtectionInspection {
    let Some(service) = current_systemd_service() else {
        return SystemdMountProtectionInspection::NotManagedBySystemd;
    };

    if service.manager == SystemdServiceManager::User {
        return SystemdMountProtectionInspection::UserManagedService {
            service: service.name,
        };
    }

    let Some(systemctl) = resolve_host_dependency_path(Path::new("systemctl")) else {
        return SystemdMountProtectionInspection::SystemctlMissing {
            service: service.name,
        };
    };

    let service_dependencies = match run_systemctl(
        &systemctl,
        service.systemctl_arguments([
            "show",
            "--all",
            "--property=Requires",
            "--property=BindsTo",
            "--property=After",
            service.name.as_str(),
        ]),
    )
    .await
    {
        Ok(output) => output,
        Err(reason) => {
            return SystemdMountProtectionInspection::QueryFailed {
                service: service.name,
                reason,
            };
        }
    };

    let mount_units = ordered_mount_units_from_service_properties(&service_dependencies);
    if mount_units.is_empty() {
        return SystemdMountProtectionInspection::Dependencies {
            service: service.name,
            mounts: Vec::new(),
        };
    }
    let mut where_arguments = Vec::with_capacity(mount_units.len() + 5);
    where_arguments.extend(["show", "--all", "--property=Id", "--property=Where"]);
    where_arguments.push("--");
    where_arguments.extend(mount_units.iter().map(String::as_str));
    let where_output =
        match run_systemctl(&systemctl, service.systemctl_arguments(where_arguments)).await {
            Ok(output) => output,
            Err(reason) => {
                return SystemdMountProtectionInspection::QueryFailed {
                    service: service.name,
                    reason,
                };
            }
        };
    let mounts = match mount_dependencies_from_properties(&mount_units, &where_output) {
        Ok(mounts) => mounts,
        Err(reason) => {
            return SystemdMountProtectionInspection::QueryFailed {
                service: service.name,
                reason,
            };
        }
    };

    SystemdMountProtectionInspection::Dependencies {
        service: service.name,
        mounts,
    }
}

#[cfg(all(not(target_os = "linux"), test))]
async fn inspect_current_process() -> SystemdMountProtectionInspection {
    SystemdMountProtectionInspection::NotManagedBySystemd
}

#[cfg(target_os = "linux")]
impl SystemdService {
    fn systemctl_arguments<'a>(
        &self,
        arguments: impl IntoIterator<Item = &'a str>,
    ) -> Vec<&'a str> {
        let mut arguments = arguments.into_iter().collect::<Vec<_>>();
        if self.manager == SystemdServiceManager::User {
            arguments.insert(0, "--user");
        }
        arguments
    }
}

#[cfg(target_os = "linux")]
fn current_systemd_service() -> Option<SystemdService> {
    let cgroups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    systemd_service_from_cgroups(&cgroups)
}

#[cfg(any(target_os = "linux", test))]
fn systemd_service_from_cgroups(cgroups: &str) -> Option<SystemdService> {
    cgroups
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, path)| path))
        .find_map(systemd_service_from_cgroup_path)
}

#[cfg(any(target_os = "linux", test))]
fn systemd_service_from_cgroup_path(path: &str) -> Option<SystemdService> {
    let units = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let manager = if units
        .iter()
        .any(|unit| is_systemd_user_manager_service(unit))
    {
        SystemdServiceManager::User
    } else {
        SystemdServiceManager::System
    };

    for unit in units.iter().rev() {
        // A scope contains processes started outside a service unit (such as a
        // login session). Do not mistake its user manager ancestor for the
        // BerryKeep server service.
        if unit.ends_with(".scope") {
            return None;
        }
        if is_systemd_service_unit(unit) && !is_systemd_user_manager_service(unit) {
            return Some(SystemdService {
                name: (*unit).to_string(),
                manager,
            });
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn is_systemd_service_unit(unit: &str) -> bool {
    unit.strip_suffix(".service")
        .is_some_and(|name| !name.is_empty())
}

#[cfg(any(target_os = "linux", test))]
fn is_systemd_user_manager_service(unit: &str) -> bool {
    unit.strip_suffix(".service")
        .is_some_and(|name| name.starts_with("user@"))
}

#[cfg(target_os = "linux")]
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

#[cfg(any(target_os = "linux", test))]
fn ordered_mount_units_from_service_properties(output: &str) -> Vec<String> {
    let mut required = BTreeSet::new();
    let mut ordered_after = BTreeSet::new();
    for (property, dependencies) in output.lines().filter_map(|line| line.split_once('=')) {
        let units = dependencies
            .split_whitespace()
            .filter_map(mount_unit_from_token);
        match property {
            "Requires" | "BindsTo" => required.extend(units),
            "After" => ordered_after.extend(units),
            _ => {}
        }
    }
    required.intersection(&ordered_after).cloned().collect()
}

#[cfg(any(target_os = "linux", test))]
fn mount_unit_from_token(token: &str) -> Option<String> {
    token.ends_with(".mount").then(|| token.to_string())
}

#[cfg(any(target_os = "linux", test))]
fn mount_dependencies_from_properties(
    mount_units: &[String],
    properties: &str,
) -> Result<Vec<SystemdMountDependency>, String> {
    let expected_units = mount_units.iter().cloned().collect::<BTreeSet<_>>();
    let mut dependencies = BTreeMap::new();
    let mut unit = None;
    let mut where_path = None;

    for line in properties.lines().chain(std::iter::once("")) {
        let line = line.trim();
        if line.is_empty() {
            finish_mount_dependency(
                &expected_units,
                &mut dependencies,
                &mut unit,
                &mut where_path,
            )?;
            continue;
        }
        let Some((property, value)) = line.split_once('=') else {
            return Err(format!("unexpected systemctl output line `{line}`"));
        };
        match property {
            "Id" if unit.replace(value.to_string()).is_some() => {
                return Err("systemctl reported Id more than once for one mount unit".to_string());
            }
            "Id" => {}
            "Where" if where_path.replace(PathBuf::from(value)).is_some() => {
                return Err(
                    "systemctl reported Where more than once for one mount unit".to_string()
                );
            }
            "Where" => {}
            _ => return Err(format!("unexpected systemctl property `{property}`")),
        }
    }

    if dependencies.len() != expected_units.len() {
        return Err("systemctl did not report every requested mount unit".to_string());
    }

    mount_units
        .iter()
        .map(|unit| {
            dependencies
                .remove(unit)
                .map(|where_path| SystemdMountDependency {
                    unit: unit.clone(),
                    where_path,
                })
                .ok_or_else(|| format!("systemctl did not report mount unit `{unit}`"))
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn finish_mount_dependency(
    expected_units: &BTreeSet<String>,
    dependencies: &mut BTreeMap<String, PathBuf>,
    unit: &mut Option<String>,
    where_path: &mut Option<PathBuf>,
) -> Result<(), String> {
    let Some(unit) = unit.take() else {
        if where_path.take().is_some() {
            return Err("systemctl reported Where without Id".to_string());
        }
        return Ok(());
    };
    let Some(where_path) = where_path.take() else {
        return Err(format!("systemctl did not report Where for `{unit}`"));
    };
    if where_path.as_os_str().is_empty() || where_path == Path::new("-") {
        return Err(format!("systemctl reported an invalid Where for `{unit}`"));
    }
    if !expected_units.contains(&unit) {
        return Err(format!("systemctl reported unexpected mount unit `{unit}`"));
    }
    if dependencies.insert(unit.clone(), where_path).is_some() {
        return Err(format!("systemctl reported `{unit}` more than once"));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn mount_protection_targets(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<MountProtectionTarget> {
    let mount_points = mount_points_for_current_process();
    let data_dir = resolved_mount_protection_path(data_dir);
    let storage_data_dir = data_dir.clone();
    let mut targets = vec![MountProtectionTarget {
        id: "systemd-mount-data-dir".to_string(),
        feature: "Systemd mount protection: IRONMESH_DATA_DIR".to_string(),
        mount_point: mount_points
            .as_deref()
            .and_then(|mount_points| mount_point_for_path(&data_dir, mount_points)),
        allows_root_filesystem: true,
        path: data_dir,
        missing_severity: HostDependencySeverity::Critical,
    }];

    targets.extend(
        storage_paths
            .iter()
            .filter(|path| !matches!(path.state, StoragePathState::Disabled))
            .map(|configured_path| {
                let path = resolved_mount_protection_path(&configured_path.path);
                MountProtectionTarget {
                    id: format!("systemd-mount-storage-{}", configured_path.id),
                    feature: format!(
                        "Systemd mount protection: storage pool `{}` ({})",
                        configured_path.id,
                        storage_path_state_label(configured_path.state)
                    ),
                    mount_point: mount_points
                        .as_deref()
                        .and_then(|mount_points| mount_point_for_path(&path, mount_points)),
                    allows_root_filesystem: path == storage_data_dir || path == Path::new("/"),
                    path,
                    missing_severity: match configured_path.state {
                        StoragePathState::Active => HostDependencySeverity::Critical,
                        StoragePathState::Draining => HostDependencySeverity::Warning,
                        StoragePathState::Disabled => HostDependencySeverity::Info,
                    },
                }
            }),
    );
    targets
}

#[cfg(any(target_os = "linux", test))]
async fn mount_protection_targets_for_current_process(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Result<Vec<MountProtectionTarget>, String> {
    let data_dir = data_dir.to_path_buf();
    let storage_paths = storage_paths.to_vec();
    tokio::task::spawn_blocking(move || mount_protection_targets(&data_dir, &storage_paths))
        .await
        .map_err(|error| format!("mount protection path inspection failed: {error}"))
}

#[cfg(any(target_os = "linux", test))]
fn resolved_mount_protection_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| absolutize_mount_protection_path(path))
}

#[cfg(any(target_os = "linux", test))]
fn absolutize_mount_protection_path(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(any(target_os = "linux", test))]
fn mount_points_for_current_process() -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .map(|contents| mount_points_from_mountinfo(&contents))
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn unescape_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(any(target_os = "linux", test))]
fn mount_point_for_path(path: &Path, mount_points: &[PathBuf]) -> Option<PathBuf> {
    mount_points
        .iter()
        .filter(|mount_point| path.starts_with(mount_point))
        .max_by_key(|mount_point| mount_point.components().count())
        .cloned()
}

#[cfg(any(target_os = "linux", test))]
fn storage_path_state_label(state: StoragePathState) -> &'static str {
    match state {
        StoragePathState::Active => "active",
        StoragePathState::Draining => "draining",
        StoragePathState::Disabled => "disabled",
    }
}

#[cfg(any(target_os = "linux", test))]
fn checks_for_inspection(
    targets: &[MountProtectionTarget],
    inspection: SystemdMountProtectionInspection,
) -> Vec<HostDependencyCheck> {
    match inspection {
        SystemdMountProtectionInspection::NotManagedBySystemd => {
            vec![not_managed_by_systemd_check()]
        }
        #[cfg(target_os = "linux")]
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
        #[cfg(any(target_os = "linux", test))]
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
        #[cfg(any(target_os = "linux", test))]
        SystemdMountProtectionInspection::UserManagedService { service } => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::NotApplicable,
                severity: HostDependencySeverity::Info,
                summary: format!(
                    "The running service `{service}` is managed by a systemd user manager"
                ),
                detail: "Systemd user managers do not manage mount units, so a user service cannot express the system-level mount protection checked for server nodes.".to_string(),
                configured_path: Some(service),
                resolved_path: None,
                install_hint: None,
            }]
        }
        #[cfg(any(target_os = "linux", test))]
        SystemdMountProtectionInspection::Dependencies { service, mounts } => targets
            .iter()
            .map(|target| {
                let protecting_mount = protecting_mount_dependency(target, &mounts);
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
                    None if target.mount_point.as_deref() == Some(Path::new("/")) && target.allows_root_filesystem => HostDependencyCheck {
                        id: target.id.clone(),
                        feature: target.feature.clone(),
                        status: HostDependencyStatus::NotApplicable,
                        severity: HostDependencySeverity::Info,
                        summary: format!(
                            "{} resides on the root filesystem",
                            target.path.display()
                        ),
                        detail: "No separate mount needs systemd protection because the path currently resolves to the root filesystem, which is required for the service to run.".to_string(),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: Some("/".to_string()),
                        install_hint: None,
                    },
                    None if target.mount_point.as_deref() == Some(Path::new("/")) => HostDependencyCheck {
                        id: target.id.clone(),
                        feature: target.feature.clone(),
                        status: HostDependencyStatus::Missing,
                        severity: target.missing_severity,
                        summary: format!(
                            "{} is currently served by the root filesystem",
                            target.path.display()
                        ),
                        detail: format!(
                            "This storage path is not currently on a separate mount, so `RequiresMountsFor={}` would only depend on the root filesystem and cannot protect the intended storage device.",
                            target.path.display()
                        ),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: Some("/".to_string()),
                        install_hint: Some(format!(
                            "Mount the intended filesystem at {}, then add `RequiresMountsFor={}` to the [Unit] section of a drop-in for `{service}`, run `sudo systemctl daemon-reload`, and restart the service.",
                            target.path.display(),
                            target.path.display()
                        )),
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

#[cfg(any(target_os = "linux", test))]
fn protecting_mount_dependency<'a>(
    target: &MountProtectionTarget,
    mounts: &'a [SystemdMountDependency],
) -> Option<&'a SystemdMountDependency> {
    match target.mount_point.as_deref() {
        Some(mount_point) if mount_point == Path::new("/") => None,
        Some(mount_point) => mounts.iter().find(|mount| mount.where_path == mount_point),
        None => mounts
            .iter()
            .filter(|mount| {
                mount.where_path != Path::new("/") && target.path.starts_with(&mount.where_path)
            })
            .max_by_key(|mount| mount.where_path.components().count()),
    }
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

    fn targets_without_known_mount_point(targets: &mut [MountProtectionTarget]) {
        for target in targets {
            target.mount_point = None;
        }
    }

    #[test]
    fn detects_the_actual_systemd_service_from_the_process_cgroup() {
        let service =
            systemd_service_from_cgroups("0::/system.slice/berrykeep-server-node.service\n");

        assert_eq!(
            service.as_ref().map(|service| service.name.as_str()),
            Some("berrykeep-server-node.service")
        );
        assert_eq!(
            service.map(|service| service.manager),
            Some(SystemdServiceManager::System)
        );
    }

    #[test]
    fn detects_a_service_from_a_nested_service_cgroup() {
        let service =
            systemd_service_from_cgroups("0::/system.slice/berrykeep-server-node.service/worker\n");

        assert_eq!(
            service.as_ref().map(|service| service.name.as_str()),
            Some("berrykeep-server-node.service")
        );
        assert_eq!(
            service.map(|service| service.manager),
            Some(SystemdServiceManager::System)
        );
    }

    #[test]
    fn user_managed_service_is_not_applicable_for_mount_protection() {
        let service = systemd_service_from_cgroups(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/berrykeep-server-node.service\n",
        )
        .unwrap();

        assert_eq!(service.name, "berrykeep-server-node.service");
        assert_eq!(service.manager, SystemdServiceManager::User);
        let checks = checks_for_inspection(
            &[],
            SystemdMountProtectionInspection::UserManagedService {
                service: service.name,
            },
        );
        assert_eq!(checks[0].status, HostDependencyStatus::NotApplicable);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
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
        let mut targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[storage_path(
                "primary",
                "/mnt/primary",
                StoragePathState::Active,
            )],
        );
        targets_without_known_mount_point(&mut targets);
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
        let mut targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[storage_path(
                "primary",
                "/mnt/primary",
                StoragePathState::Active,
            )],
        );
        targets_without_known_mount_point(&mut targets);
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
        let mut targets = mount_protection_targets(
            Path::new("/srv/berrykeep"),
            &[
                storage_path("primary", "/mnt/primary", StoragePathState::Active),
                storage_path("archive", "/mnt/archive", StoragePathState::Draining),
                storage_path("retired", "/mnt/retired", StoragePathState::Disabled),
            ],
        );
        targets_without_known_mount_point(&mut targets);
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
    fn effective_mount_dependencies_require_a_mount_and_start_order() {
        let units = ordered_mount_units_from_service_properties(
            "Requires=mnt-primary.mount sysinit.target\nBindsTo=var-lib-berrykeep.mount\nAfter=mnt-primary.mount var-lib-berrykeep.mount unrelated.mount\nWants=unrelated.mount\n",
        );

        assert_eq!(units, vec!["mnt-primary.mount", "var-lib-berrykeep.mount"]);
    }

    #[test]
    fn mount_requirement_without_start_order_is_not_protected() {
        let units = ordered_mount_units_from_service_properties(
            "Requires=mnt-primary.mount\nAfter=network.target\n",
        );

        assert!(units.is_empty());
    }

    #[test]
    fn mount_properties_are_matched_by_unit_id_in_one_systemctl_response() {
        let mounts = mount_dependencies_from_properties(
            &[
                "mnt-primary.mount".to_string(),
                "mnt-archive.mount".to_string(),
            ],
            "Where=/mnt/archive\nId=mnt-archive.mount\n\nWhere=/mnt/primary\nId=mnt-primary.mount\n",
        )
        .unwrap();

        assert_eq!(
            mounts,
            vec![
                systemd_mount("mnt-primary.mount", "/mnt/primary"),
                systemd_mount("mnt-archive.mount", "/mnt/archive"),
            ]
        );
    }

    #[test]
    fn missing_mount_properties_fail_closed() {
        let error = mount_dependencies_from_properties(
            &["mnt-primary.mount".to_string()],
            "Id=mnt-primary.mount\n\n",
        )
        .unwrap_err();

        assert!(error.contains("Where"));
    }

    #[test]
    fn root_filesystem_exemption_does_not_mask_a_distinct_storage_pool() {
        let targets = vec![
            MountProtectionTarget {
                id: "systemd-mount-data-dir".to_string(),
                feature: "Systemd mount protection: IRONMESH_DATA_DIR".to_string(),
                path: PathBuf::from("/var/lib/berrykeep"),
                mount_point: Some(PathBuf::from("/")),
                allows_root_filesystem: true,
                missing_severity: HostDependencySeverity::Critical,
            },
            MountProtectionTarget {
                id: "systemd-mount-storage-legacy-primary".to_string(),
                feature: "Systemd mount protection: storage pool `legacy-primary` (active)"
                    .to_string(),
                path: PathBuf::from("/var/lib/berrykeep"),
                mount_point: Some(PathBuf::from("/")),
                allows_root_filesystem: true,
                missing_severity: HostDependencySeverity::Critical,
            },
            MountProtectionTarget {
                id: "systemd-mount-storage-primary".to_string(),
                feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
                path: PathBuf::from("/mnt/primary"),
                mount_point: Some(PathBuf::from("/")),
                allows_root_filesystem: false,
                missing_severity: HostDependencySeverity::Critical,
            },
        ];
        let checks = checks_for_inspection(
            &targets,
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![systemd_mount("-.mount", "/")],
            },
        );

        let root = checks
            .iter()
            .find(|check| check.id == "systemd-mount-data-dir")
            .unwrap();
        assert_eq!(root.status, HostDependencyStatus::NotApplicable);
        assert_eq!(root.severity, HostDependencySeverity::Info);
        assert!(root.install_hint.is_none());

        let storage = checks
            .iter()
            .find(|check| check.id == "systemd-mount-storage-legacy-primary")
            .unwrap();
        assert_eq!(storage.status, HostDependencyStatus::NotApplicable);
        assert_eq!(storage.severity, HostDependencySeverity::Info);
        assert!(storage.install_hint.is_none());

        let missing_storage = checks
            .iter()
            .find(|check| check.id == "systemd-mount-storage-primary")
            .unwrap();
        assert_eq!(missing_storage.status, HostDependencyStatus::Missing);
        assert_eq!(missing_storage.severity, HostDependencySeverity::Critical);
        assert!(
            missing_storage
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("Mount the intended filesystem")
        );
    }

    #[test]
    fn nested_storage_mount_requires_its_exact_mount_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-storage-primary".to_string(),
            feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
            path: PathBuf::from("/srv/pool/media"),
            mount_point: Some(PathBuf::from("/srv/pool")),
            allows_root_filesystem: false,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            std::slice::from_ref(&target),
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![systemd_mount("srv.mount", "/srv")],
            },
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Critical);

        let checks = checks_for_inspection(
            &[target],
            SystemdMountProtectionInspection::Dependencies {
                service: "berrykeep-server-node.service".to_string(),
                mounts: vec![systemd_mount("srv-pool.mount", "/srv/pool")],
            },
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Ready);
    }

    #[test]
    fn relative_mount_protection_paths_are_absolutized() {
        let path = resolved_mount_protection_path(Path::new("./data/server-node"));

        assert!(path.is_absolute());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mountinfo_parser_reads_the_mount_point_field() {
        let mount_points = mount_points_from_mountinfo(
            "36 25 0:32 / /mnt/storage rw,nosuid,nodev - ext4 /dev/sda1 rw\n",
        );

        assert_eq!(mount_points, vec![PathBuf::from("/mnt/storage")]);
    }
}
