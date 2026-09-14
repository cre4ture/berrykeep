#[cfg(any(target_os = "linux", test))]
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
#[cfg(any(target_os = "linux", test))]
use std::path::{Component, PathBuf};
#[cfg(any(target_os = "linux", test))]
use std::process::Stdio;
#[cfg(all(target_os = "linux", not(test)))]
use std::sync::OnceLock;
#[cfg(any(target_os = "linux", test))]
use std::time::Duration;
#[cfg(all(target_os = "linux", not(test)))]
use std::time::Instant;

#[cfg(any(target_os = "linux", test))]
use tokio::process::Command;
#[cfg(any(target_os = "linux", test))]
use tokio::time::timeout;

#[cfg(any(target_os = "linux", test))]
use futures_util::future::join_all;

use super::StoragePathConfig;
#[cfg(any(target_os = "linux", test))]
use super::StoragePathState;
use super::media_tools::{HostDependencyCheck, HostDependencySeverity, HostDependencyStatus};

#[cfg(target_os = "linux")]
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);
// `systemctl show` omits inactive units during glob expansion unless `--all` is
// present. Keep inactive generated/fstab mount units visible so an unmounted
// expected filesystem is recognized as a root fallback, not as root-backed.
#[cfg(target_os = "linux")]
const SYSTEMCTL_INCLUDE_INACTIVE_UNITS: &str = "--all";
#[cfg(any(target_os = "linux", test))]
const PATH_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", not(test)))]
const MOUNT_PROTECTION_CACHE_TTL: Duration = Duration::from_secs(30);
#[cfg(all(target_os = "linux", not(test)))]
const MOUNT_PROTECTION_INSPECTION_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemdMountProtectionInspection {
    NotManagedBySystemd,
    #[cfg(target_os = "linux")]
    SystemctlMissing {
        service: String,
    },
    #[cfg(any(target_os = "linux", test))]
    PathCanonicalizerUnavailable {
        service: String,
        reason: String,
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
        host_mount_points: BTreeSet<PathBuf>,
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
struct MountPoint {
    device: String,
    path: PathBuf,
    root: PathBuf,
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
    mount_point_is_bind: bool,
    backing_mount_points: Vec<PathBuf>,
    path_resolution_error: Option<String>,
    missing_severity: HostDependencySeverity,
}

pub(super) async fn mount_protection_checks(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<HostDependencyCheck> {
    #[cfg(all(target_os = "linux", not(test)))]
    {
        return cached_mount_protection_checks(data_dir, storage_paths).await;
    }

    #[cfg(test)]
    {
        return mount_protection_checks_for_current_process(data_dir, storage_paths).await;
    }

    #[cfg(not(any(target_os = "linux", test)))]
    {
        let _ = (data_dir, storage_paths);
        vec![not_managed_by_systemd_check()]
    }
}

#[cfg(all(target_os = "linux", not(test)))]
#[derive(Clone, PartialEq, Eq)]
struct MountProtectionCacheKey {
    data_dir: PathBuf,
    storage_paths: Vec<StoragePathConfig>,
}

#[cfg(all(target_os = "linux", not(test)))]
struct MountProtectionCacheEntry {
    key: MountProtectionCacheKey,
    checked_at: Instant,
    checks: Vec<HostDependencyCheck>,
}

#[cfg(all(target_os = "linux", not(test)))]
static MOUNT_PROTECTION_CACHE: OnceLock<tokio::sync::Mutex<Option<MountProtectionCacheEntry>>> =
    OnceLock::new();

#[cfg(all(target_os = "linux", not(test)))]
async fn cached_mount_protection_checks(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<HostDependencyCheck> {
    let key = MountProtectionCacheKey {
        data_dir: data_dir.to_path_buf(),
        storage_paths: storage_paths.to_vec(),
    };
    let cache = MOUNT_PROTECTION_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cache = cache.lock().await;
    if let Some(entry) = cache.as_ref()
        && entry.key == key
        && entry.checked_at.elapsed() <= MOUNT_PROTECTION_CACHE_TTL
    {
        return entry.checks.clone();
    }
    let checks = timeout(
        MOUNT_PROTECTION_INSPECTION_TIMEOUT,
        mount_protection_checks_for_current_process(data_dir, storage_paths),
    )
    .await
    .unwrap_or_else(|_| mount_protection_inspection_timed_out_checks());
    *cache = Some(MountProtectionCacheEntry {
        key,
        checked_at: Instant::now(),
        checks: checks.clone(),
    });
    checks
}

#[cfg(all(target_os = "linux", not(test)))]
fn mount_protection_inspection_timed_out_checks() -> Vec<HostDependencyCheck> {
    vec![HostDependencyCheck {
        id: "systemd-mount-protection".to_string(),
        feature: "Systemd mount protection".to_string(),
        status: HostDependencyStatus::Missing,
        severity: HostDependencySeverity::Info,
        summary: "Could not finish checking systemd mount protection in time".to_string(),
        detail: format!(
            "The systemd dependency inspection exceeded {} seconds. Retry after the host is responsive.",
            MOUNT_PROTECTION_INSPECTION_TIMEOUT.as_secs()
        ),
        configured_path: None,
        resolved_path: None,
        install_hint: None,
    }]
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
            let canonicalizer = match usable_path_canonicalizer().await {
                Ok(canonicalizer) => canonicalizer,
                Err(reason) => {
                    return checks_for_inspection(
                        &[],
                        SystemdMountProtectionInspection::PathCanonicalizerUnavailable {
                            service: service.clone(),
                            reason,
                        },
                    );
                }
            };
            match mount_protection_targets_for_current_process(
                data_dir,
                storage_paths,
                &canonicalizer,
            )
            .await
            {
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

    let Some(systemctl) = resolve_mount_protection_tool(Path::new("systemctl")).await else {
        return SystemdMountProtectionInspection::SystemctlMissing {
            service: service.name,
        };
    };

    let service_dependencies = match run_systemctl(
        &systemctl,
        [
            "show",
            SYSTEMCTL_INCLUDE_INACTIVE_UNITS,
            "--property=Requires",
            "--property=BindsTo",
            "--property=After",
            "--",
            service.name.as_str(),
        ],
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
    let mounts = if mount_units.is_empty() {
        Vec::new()
    } else {
        let mut where_arguments = Vec::with_capacity(mount_units.len() + 5);
        where_arguments.extend([
            "show",
            SYSTEMCTL_INCLUDE_INACTIVE_UNITS,
            "--property=Id",
            "--property=Where",
        ]);
        where_arguments.push("--");
        where_arguments.extend(mount_units.iter().map(String::as_str));
        let where_output = match run_systemctl(&systemctl, where_arguments).await {
            Ok(output) => output,
            Err(reason) => {
                return SystemdMountProtectionInspection::QueryFailed {
                    service: service.name,
                    reason,
                };
            }
        };
        match mount_dependencies_from_properties(&mount_units, &where_output) {
            Ok(mounts) => mounts,
            Err(reason) => {
                return SystemdMountProtectionInspection::QueryFailed {
                    service: service.name,
                    reason,
                };
            }
        }
    };
    let host_mount_points = match run_systemctl(
        &systemctl,
        [
            "show",
            SYSTEMCTL_INCLUDE_INACTIVE_UNITS,
            "--property=Id",
            "--property=Where",
            "--",
            "*.mount",
        ],
    )
    .await
    {
        Ok(output) => match nonempty_host_mount_points_from_properties(&output) {
            Ok(mount_points) => mount_points,
            Err(reason) => {
                return SystemdMountProtectionInspection::QueryFailed {
                    service: service.name,
                    reason,
                };
            }
        },
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
        host_mount_points,
    }
}

#[cfg(all(not(target_os = "linux"), test))]
async fn inspect_current_process() -> SystemdMountProtectionInspection {
    SystemdMountProtectionInspection::NotManagedBySystemd
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
    for_each_mount_unit_record(properties, |unit, where_path| {
        let Some(where_path) = where_path else {
            return Err(format!("systemctl did not report Where for `{unit}`"));
        };
        if !expected_units.contains(&unit) {
            return Err(format!("systemctl reported unexpected mount unit `{unit}`"));
        }
        if where_path.as_os_str().is_empty() || where_path == Path::new("-") {
            return Ok(());
        }
        if dependencies.insert(unit.clone(), where_path).is_some() {
            return Err(format!("systemctl reported `{unit}` more than once"));
        }
        Ok(())
    })?;

    Ok(mount_units
        .iter()
        .filter_map(|unit| {
            dependencies
                .remove(unit)
                .map(|where_path| SystemdMountDependency {
                    unit: unit.clone(),
                    where_path,
                })
        })
        .collect())
}

#[cfg(any(target_os = "linux", test))]
fn host_mount_points_from_properties(properties: &str) -> Result<BTreeSet<PathBuf>, String> {
    let mut mount_points = BTreeSet::new();
    for_each_mount_unit_record(properties, |unit, where_path| {
        if !unit.ends_with(".mount") {
            return Err(format!("systemctl reported non-mount unit `{unit}`"));
        }
        let Some(where_path) = where_path else {
            return Ok(());
        };
        if where_path.as_os_str().is_empty() || where_path == Path::new("-") {
            return Ok(());
        }
        mount_points.insert(where_path);
        Ok(())
    })?;

    Ok(mount_points)
}

#[cfg(any(target_os = "linux", test))]
fn nonempty_host_mount_points_from_properties(
    properties: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let mount_points = host_mount_points_from_properties(properties)?;
    if mount_points.is_empty() {
        Err("systemctl did not report any host mount units".to_string())
    } else {
        Ok(mount_points)
    }
}

#[cfg(any(target_os = "linux", test))]
fn for_each_mount_unit_record(
    properties: &str,
    mut visit: impl FnMut(String, Option<PathBuf>) -> Result<(), String>,
) -> Result<(), String> {
    let mut unit = None;
    let mut where_path = None;
    for line in properties.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            let Some(unit) = unit.take() else {
                if where_path.take().is_some() {
                    return Err("systemctl reported Where without Id".to_string());
                }
                continue;
            };
            visit(unit, where_path.take())?;
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
    Ok(())
}

#[cfg(test)]
fn mount_protection_targets(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
) -> Vec<MountProtectionTarget> {
    mount_protection_targets_with_path_resolution(data_dir, storage_paths, None, &BTreeMap::new())
}

#[cfg(any(target_os = "linux", test))]
fn mount_protection_targets_with_path_resolution(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
    data_dir_resolution_error: Option<String>,
    storage_path_resolution_errors: &BTreeMap<String, String>,
) -> Vec<MountProtectionTarget> {
    let mount_points = mount_points_for_current_process().unwrap_or_default();
    let data_dir = normalized_mount_protection_path(data_dir);
    let data_dir_mount_point = mount_point_for_path(&data_dir, &mount_points);
    let data_dir_backing_mount_points = data_dir_mount_point
        .filter(|mount_point| mount_point.root != Path::new("/"))
        .map(|mount_point| backing_mount_points_for_path(&data_dir, mount_point, &mount_points))
        .unwrap_or_default();
    let mut protected_paths = BTreeSet::from([data_dir.clone()]);
    let mut targets = vec![MountProtectionTarget {
        id: "systemd-mount-data-dir".to_string(),
        feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
        mount_point: data_dir_mount_point.map(|mount_point| mount_point.path.clone()),
        mount_point_is_bind: data_dir_mount_point
            .is_some_and(|mount_point| mount_point.root != Path::new("/")),
        backing_mount_points: data_dir_backing_mount_points,
        path_resolution_error: data_dir_resolution_error,
        path: data_dir,
        missing_severity: HostDependencySeverity::Critical,
    }];

    targets.extend(
        storage_paths
            .iter()
            .filter(|path| !matches!(path.state, StoragePathState::Disabled))
            .filter_map(|configured_path| {
                let path = normalized_mount_protection_path(&configured_path.path);
                if !protected_paths.insert(path.clone()) {
                    return None;
                }
                let mount_point = mount_point_for_path(&path, &mount_points);
                let backing_mount_points = mount_point
                    .filter(|mount_point| mount_point.root != Path::new("/"))
                    .map(|mount_point| {
                        backing_mount_points_for_path(&path, mount_point, &mount_points)
                    })
                    .unwrap_or_default();
                Some(MountProtectionTarget {
                    id: format!("systemd-mount-storage-{}", configured_path.id),
                    feature: format!(
                        "Systemd mount protection: storage pool `{}` ({})",
                        configured_path.id,
                        storage_path_state_label(configured_path.state)
                    ),
                    mount_point: mount_point.map(|mount_point| mount_point.path.clone()),
                    mount_point_is_bind: mount_point
                        .is_some_and(|mount_point| mount_point.root != Path::new("/")),
                    backing_mount_points,
                    path_resolution_error: storage_path_resolution_errors
                        .get(&configured_path.id)
                        .cloned(),
                    path,
                    missing_severity: match configured_path.state {
                        StoragePathState::Active => HostDependencySeverity::Critical,
                        StoragePathState::Draining => HostDependencySeverity::Warning,
                        StoragePathState::Disabled => HostDependencySeverity::Info,
                    },
                })
            }),
    );
    targets
}

#[cfg(any(target_os = "linux", test))]
async fn mount_protection_targets_for_current_process(
    data_dir: &Path,
    storage_paths: &[StoragePathConfig],
    canonicalizer: &Path,
) -> Result<Vec<MountProtectionTarget>, String> {
    let storage_path_futures = storage_paths
        .iter()
        .filter(|storage_path| !matches!(storage_path.state, StoragePathState::Disabled))
        .map(|storage_path| async {
            (
                storage_path.id.clone(),
                resolve_mount_protection_path(&storage_path.path, canonicalizer).await,
            )
        });
    let (data_dir, resolved_storage_paths) = tokio::join!(
        resolve_mount_protection_path(data_dir, canonicalizer),
        join_all(storage_path_futures),
    );
    let resolved_storage_paths = resolved_storage_paths
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let mut storage_paths = storage_paths.to_vec();
    let mut storage_path_resolution_errors = BTreeMap::new();
    for storage_path in &mut storage_paths {
        let Some(resolved_path) = resolved_storage_paths.get(&storage_path.id) else {
            continue;
        };
        if let Some(error) = &resolved_path.error {
            storage_path_resolution_errors.insert(storage_path.id.clone(), error.clone());
        }
        storage_path.path = resolved_path.path.clone();
    }
    tokio::task::spawn_blocking(move || {
        mount_protection_targets_with_path_resolution(
            &data_dir.path,
            &storage_paths,
            data_dir.error,
            &storage_path_resolution_errors,
        )
    })
    .await
    .map_err(|error| format!("mount protection path inspection failed: {error}"))
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct ResolvedMountProtectionPath {
    path: PathBuf,
    error: Option<String>,
}

#[cfg(any(target_os = "linux", test))]
async fn usable_path_canonicalizer() -> Result<PathBuf, String> {
    let Some(canonicalizer) = resolve_mount_protection_tool(Path::new("readlink")).await else {
        return Err("the `readlink` program is unavailable".to_string());
    };
    let resolved_root = resolve_mount_protection_path(Path::new("/"), &canonicalizer).await;
    match resolved_root.error {
        Some(error) => Err(format!(
            "`{}` cannot canonicalize paths: {error}",
            canonicalizer.display()
        )),
        None => Ok(canonicalizer),
    }
}

#[cfg(any(target_os = "linux", test))]
async fn resolve_mount_protection_tool(configured_path: &Path) -> Option<PathBuf> {
    // Resolve PATH inside a killable child: metadata on an unavailable
    // network-backed PATH entry can block indefinitely, while dropping a Tokio
    // blocking task cannot cancel its worker thread. The tool name is passed as
    // a positional argument rather than interpolated into the shell program.
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "command -v -- \"$1\"",
            "berrykeep-mount-protection-tool-resolution",
        ])
        .arg(configured_path)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(PATH_RESOLUTION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = readlink_output_path(&output.stdout).ok()?;
    (path.is_absolute() || path.components().count() > 1).then_some(path)
}

#[cfg(any(target_os = "linux", test))]
async fn resolve_mount_protection_path(
    path: &Path,
    canonicalizer: &Path,
) -> ResolvedMountProtectionPath {
    let lexical_path = absolutize_mount_protection_path(path);
    // Filesystem canonicalization can block indefinitely when a network mount is
    // unavailable. A child process lets the report enforce a timeout and kill the
    // blocked resolver instead of leaving a Tokio blocking thread behind.
    let mut command = Command::new(canonicalizer);
    command
        .args(["--canonicalize-existing", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(PATH_RESOLUTION_TIMEOUT, command.output()).await;
    match output {
        Ok(Ok(output)) if output.status.success() => match readlink_output_path(&output.stdout) {
            Ok(path) => ResolvedMountProtectionPath { path, error: None },
            Err(reason) => ResolvedMountProtectionPath {
                path: lexical_path,
                error: Some(format!("`{}` {reason}", canonicalizer.display())),
            },
        },
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            ResolvedMountProtectionPath {
                path: lexical_path,
                error: Some(if detail.is_empty() {
                    format!(
                        "`{}` exited with {}",
                        canonicalizer.display(),
                        output.status
                    )
                } else {
                    format!(
                        "`{}` exited with {}: {detail}",
                        canonicalizer.display(),
                        output.status
                    )
                }),
            }
        }
        Ok(Err(error)) => ResolvedMountProtectionPath {
            path: lexical_path,
            error: Some(format!(
                "failed to start `{}`: {error}",
                canonicalizer.display()
            )),
        },
        Err(_) => ResolvedMountProtectionPath {
            path: lexical_path,
            error: Some(format!(
                "`{}` did not resolve the path within {} seconds",
                canonicalizer.display(),
                PATH_RESOLUTION_TIMEOUT.as_secs()
            )),
        },
    }
}

#[cfg(any(target_os = "linux", test))]
fn readlink_output_path(output: &[u8]) -> Result<PathBuf, &'static str> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    if output.is_empty() {
        return Err("returned an empty path");
    }
    let path = std::str::from_utf8(output).map_err(|_| "returned a non-UTF-8 path")?;
    Ok(PathBuf::from(path))
}

#[cfg(any(target_os = "linux", test))]
fn normalized_mount_protection_path(path: &Path) -> PathBuf {
    absolutize_mount_protection_path(path)
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
fn mount_points_for_current_process() -> Option<Vec<MountPoint>> {
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
fn mount_points_from_mountinfo(mountinfo: &str) -> Vec<MountPoint> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _mount_id = fields.next()?;
            let _parent_id = fields.next()?;
            let device = fields.next()?;
            let root = fields.next()?;
            let mount_point = fields.next()?;
            Some(MountPoint {
                device: device.to_string(),
                path: PathBuf::from(unescape_mountinfo_path(mount_point)),
                root: PathBuf::from(unescape_mountinfo_path(root)),
            })
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
fn mount_point_for_path<'a>(path: &Path, mount_points: &'a [MountPoint]) -> Option<&'a MountPoint> {
    mount_points
        .iter()
        .filter(|mount_point| path.starts_with(&mount_point.path))
        .max_by_key(|mount_point| mount_point.path.components().count())
}

#[cfg(any(target_os = "linux", test))]
fn backing_mount_points_for_path(
    path: &Path,
    mount_point: &MountPoint,
    mount_points: &[MountPoint],
) -> Vec<PathBuf> {
    let Some(relative_path) = path.strip_prefix(&mount_point.path).ok() else {
        return Vec::new();
    };
    let filesystem_path = mount_point.root.join(relative_path);
    let mut backing_mount_points = mount_points
        .iter()
        .filter(|candidate| {
            candidate.device == mount_point.device
                && candidate.path != mount_point.path
                // A bind created solely in the service namespace can map a
                // root-backed path onto itself. Keep root as its backing source
                // so the caller can classify that case as not applicable. A
                // subvolume bind has distinct source and target paths, and must
                // never use the root mount as its backing dependency.
                && (candidate.path != Path::new("/") || mount_point.root == mount_point.path)
        })
        .filter(|candidate| filesystem_path.starts_with(&candidate.root))
        .map(|candidate| candidate.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    backing_mount_points.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    backing_mount_points
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
fn requires_mounts_for_remedy(service: &str, path: &Path) -> String {
    format!(
        "Ensure the filesystem is declared in /etc/fstab or by a native .mount unit, then add `RequiresMountsFor={}` to the [Unit] section of a drop-in for `{service}` so systemd waits for and requires the mount before startup. If the service must also stop when that mount becomes inactive, bind it to the relevant .mount unit with `BindsTo=`. Run `sudo systemctl daemon-reload`, then restart the service.",
        path.display()
    )
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
        SystemdMountProtectionInspection::PathCanonicalizerUnavailable { service, reason } => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::Missing,
                severity: HostDependencySeverity::Info,
                summary: format!(
                    "The running service `{service}` was detected, but storage paths cannot be canonicalized"
                ),
                detail: format!(
                    "The server is managed by systemd, but mount protection cannot be inspected until storage paths can be resolved safely: {reason}."
                ),
                configured_path: Some("readlink".to_string()),
                resolved_path: None,
                install_hint: Some("Install or restore a `readlink` implementation that supports `--canonicalize-existing`, then refresh this report.".to_string()),
            }]
        }
        #[cfg(any(target_os = "linux", test))]
        SystemdMountProtectionInspection::QueryFailed { service, reason } => {
            vec![HostDependencyCheck {
                id: "systemd-mount-protection".to_string(),
                feature: "Systemd mount protection".to_string(),
                status: HostDependencyStatus::Missing,
                // An unreadable systemd graph means the target is unverified,
                // not that a specific mount dependency is absent. Preserve the
                // configured warning/critical severities for confirmed per-path
                // gaps and keep this diagnostic off the dashboard.
                severity: HostDependencySeverity::Info,
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
        SystemdMountProtectionInspection::Dependencies {
            service,
            mounts,
            host_mount_points,
        } => targets
            .iter()
            .map(|target| {
                let expected_host_mount = expected_host_mount_point(target, &host_mount_points);
                let is_root_backed_namespace_bind = target.mount_point_is_bind
                    && expected_host_mount.is_none()
                    && target
                        .backing_mount_points
                        .iter()
                        .any(|mount_point| mount_point == Path::new("/"));
                let target_is_on_expected_host_mount = match (
                    target.mount_point.as_deref(),
                    expected_host_mount.map(PathBuf::as_path),
                ) {
                    (Some(actual), Some(expected)) => actual.starts_with(expected),
                    _ => true,
                };
                let can_use_backing_mount_dependency = match (
                    target.mount_point.as_deref(),
                    expected_host_mount.map(PathBuf::as_path),
                ) {
                    // A host bind mount has its own loaded mount unit. Its backing
                    // filesystem cannot protect the target when that bind mount fails.
                    (Some(actual), Some(expected)) => actual != expected,
                    // A bind mount created inside the service namespace has no host
                    // mount unit at its target, so its backing source is authoritative.
                    _ => true,
                };
                let protecting_mount = (target.path_resolution_error.is_none()
                    && target_is_on_expected_host_mount
                    && !is_root_backed_namespace_bind)
                    .then(|| {
                        protecting_mount_dependency(
                            target,
                            &mounts,
                            can_use_backing_mount_dependency,
                        )
                    })
                    .flatten();
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
                    None if target.path_resolution_error.is_some() => HostDependencyCheck {
                        id: target.id.clone(),
                        feature: target.feature.clone(),
                        status: HostDependencyStatus::Missing,
                        severity: HostDependencySeverity::Info,
                        summary: format!(
                            "Could not resolve the filesystem path for {}",
                            target.path.display()
                        ),
                        detail: format!(
                            "The storage path could not be resolved to its physical filesystem: {}. Mount protection has not been verified, so the path is not treated as root-backed or as a confirmed protection gap.",
                            target
                                .path_resolution_error
                                .as_deref()
                                .unwrap_or("unknown resolution failure")
                        ),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: None,
                        install_hint: Some(format!(
                            "Retry once {} is responsive. If this persists, confirm that the path resolves to the intended storage filesystem before changing its service dependencies.",
                            target.path.display(),
                        )),
                    },
                    None
                        if target.mount_point.as_deref() == Some(Path::new("/"))
                            && expected_host_mount.is_some() =>
                    {
                        HostDependencyCheck {
                            id: target.id.clone(),
                            feature: target.feature.clone(),
                            status: HostDependencyStatus::Missing,
                            severity: target.missing_severity,
                            summary: format!(
                                "{} is currently served by the root filesystem",
                                target.path.display()
                            ),
                            detail: format!(
                                "Systemd has a loaded mount unit at or above this path, but the configured storage path currently falls back to the root filesystem. `RequiresMountsFor={}` would otherwise only depend on the root filesystem and cannot require the intended storage device during startup.",
                                target.path.display()
                            ),
                            configured_path: Some(target.path.display().to_string()),
                            resolved_path: Some("/".to_string()),
                            install_hint: Some(format!(
                                "Mount the filesystem declared for {}. {}",
                                target.path.display(),
                                requires_mounts_for_remedy(&service, &target.path)
                            )),
                        }
                    }
                    None
                        if !target_is_on_expected_host_mount
                            && target.mount_point.as_deref() != Some(Path::new("/")) =>
                    {
                        let expected_mount = expected_host_mount
                            .expect("an unexpected live mount requires an expected host mount");
                        let actual_mount = target
                            .mount_point
                            .as_deref()
                            .expect("a live mount is required to detect an unexpected mount");
                        HostDependencyCheck {
                            id: target.id.clone(),
                            feature: target.feature.clone(),
                            status: HostDependencyStatus::Missing,
                            severity: target.missing_severity,
                            summary: format!(
                                "{} is currently served by {} instead of {}",
                                target.path.display(),
                                actual_mount.display(),
                                expected_mount.display()
                            ),
                            detail: format!(
                                "Systemd has a loaded mount unit for {}, but the configured storage path is currently falling back to {}. Its mount protection cannot be verified until the expected filesystem is mounted.",
                                expected_mount.display(),
                                actual_mount.display()
                            ),
                            configured_path: Some(target.path.display().to_string()),
                            resolved_path: Some(actual_mount.display().to_string()),
                            install_hint: Some(format!(
                                "Restore the filesystem declared for {}, ensure the drop-in for `{service}` contains `RequiresMountsFor={}`, run `sudo systemctl daemon-reload`, and restart the service.",
                                expected_mount.display(),
                                target.path.display()
                            )),
                        }
                    }
                    None
                        if expected_host_mount.is_none()
                            && (target.mount_point.as_deref() == Some(Path::new("/"))
                                || is_root_backed_namespace_bind) =>
                    HostDependencyCheck {
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
                            "If the filesystem containing this path is unavailable during startup, `{service}` can start without systemd waiting for and requiring the intended mount."
                        ),
                        configured_path: Some(target.path.display().to_string()),
                        resolved_path: None,
                        install_hint: Some(requires_mounts_for_remedy(&service, &target.path)),
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
    can_use_backing_mount_dependency: bool,
) -> Option<&'a SystemdMountDependency> {
    match target.mount_point.as_deref() {
        Some(mount_point) if mount_point == Path::new("/") => None,
        Some(mount_point) => {
            let direct_mount = mounts.iter().find(|mount| mount.where_path == mount_point);
            if direct_mount.is_some()
                || !target.mount_point_is_bind
                || !can_use_backing_mount_dependency
            {
                return direct_mount;
            }
            target
                .backing_mount_points
                .iter()
                .find_map(|backing_mount_point| {
                    mounts
                        .iter()
                        .find(|mount| mount.where_path == *backing_mount_point)
                })
        }
        None => mounts
            .iter()
            .filter(|mount| {
                mount.where_path != Path::new("/") && target.path.starts_with(&mount.where_path)
            })
            .max_by_key(|mount| mount.where_path.components().count()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn expected_host_mount_point<'a>(
    target: &MountProtectionTarget,
    host_mount_points: &'a BTreeSet<PathBuf>,
) -> Option<&'a PathBuf> {
    host_mount_points
        .iter()
        .filter(|mount_point| {
            *mount_point != Path::new("/") && target.path.starts_with(mount_point)
        })
        .max_by_key(|mount_point| mount_point.components().count())
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

    fn dependencies(mounts: Vec<SystemdMountDependency>) -> SystemdMountProtectionInspection {
        let host_mount_points = mounts
            .iter()
            .map(|mount| mount.where_path.clone())
            .collect();
        SystemdMountProtectionInspection::Dependencies {
            service: "berrykeep-server-node.service".to_string(),
            mounts,
            host_mount_points,
        }
    }

    fn dependencies_with_host_mount_points(
        mounts: Vec<SystemdMountDependency>,
        host_mount_points: &[&str],
    ) -> SystemdMountProtectionInspection {
        SystemdMountProtectionInspection::Dependencies {
            service: "berrykeep-server-node.service".to_string(),
            mounts,
            host_mount_points: host_mount_points.iter().map(PathBuf::from).collect(),
        }
    }

    fn targets_without_known_mount_point(targets: &mut [MountProtectionTarget]) {
        for target in targets {
            target.mount_point = None;
            target.mount_point_is_bind = false;
            target.backing_mount_points.clear();
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
    #[cfg(target_os = "linux")]
    fn missing_systemctl_remains_an_informational_host_tool_finding() {
        let checks = checks_for_inspection(
            &[],
            SystemdMountProtectionInspection::SystemctlMissing {
                service: "berrykeep-server-node.service".to_string(),
            },
        );

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
    }

    #[test]
    fn unavailable_path_canonicalizer_remains_an_informational_host_tool_finding() {
        let checks = checks_for_inspection(
            &[],
            SystemdMountProtectionInspection::PathCanonicalizerUnavailable {
                service: "berrykeep-server-node.service".to_string(),
                reason: "the `readlink` program is unavailable".to_string(),
            },
        );

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
    }

    #[test]
    fn failed_systemd_inspection_remains_informational() {
        let checks = checks_for_inspection(
            &[],
            SystemdMountProtectionInspection::QueryFailed {
                service: "berrykeep-server-node.service".to_string(),
                reason: "systemctl did not respond".to_string(),
            },
        );

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
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
            dependencies(vec![
                systemd_mount("srv.mount", "/srv"),
                systemd_mount("mnt-primary.mount", "/mnt/primary"),
            ]),
        );

        assert_eq!(checks.len(), 2);
        assert!(
            checks
                .iter()
                .all(|check| check.status == HostDependencyStatus::Ready)
        );
    }

    #[test]
    fn storage_path_matching_data_dir_is_checked_once() {
        let mut targets = mount_protection_targets(
            Path::new("/var/lib/berrykeep"),
            &[
                storage_path(
                    "legacy-primary",
                    "/var/lib/berrykeep/.",
                    StoragePathState::Active,
                ),
                storage_path("archive", "/mnt/archive", StoragePathState::Draining),
            ],
        );
        targets_without_known_mount_point(&mut targets);
        let checks = checks_for_inspection(
            &targets,
            dependencies(vec![systemd_mount("archive.mount", "/mnt/archive")]),
        );

        assert_eq!(checks.len(), 2);
        assert!(
            checks
                .iter()
                .any(|check| check.id == "systemd-mount-data-dir")
        );
        assert_eq!(
            checks
                .iter()
                .find(|check| check.id == "systemd-mount-data-dir")
                .unwrap()
                .severity,
            HostDependencySeverity::Critical
        );
        assert!(
            checks
                .iter()
                .all(|check| check.id != "systemd-mount-storage-legacy-primary")
        );
        assert!(
            checks
                .iter()
                .any(|check| check.id == "systemd-mount-storage-archive")
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
            dependencies(vec![
                systemd_mount("srv.mount", "/srv"),
                systemd_mount("mnt-primary.mount", "/mnt/primary"),
            ]),
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
            draining
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("BindsTo=")
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
    fn systemctl_properties_preserve_whitespace_in_mount_paths() {
        let mounts = mount_dependencies_from_properties(
            &["mnt-primary.mount".to_string()],
            "Id=mnt-primary.mount\nWhere=/mnt/primary \n",
        )
        .unwrap();
        assert_eq!(
            mounts,
            vec![systemd_mount("mnt-primary.mount", "/mnt/primary ")]
        );

        let mount_points =
            host_mount_points_from_properties("Id=mnt-primary.mount\nWhere=/mnt/primary \n")
                .unwrap();
        assert_eq!(
            mount_points,
            BTreeSet::from([PathBuf::from("/mnt/primary ")])
        );
    }

    #[test]
    fn unavailable_mount_units_are_ignored_without_discarding_other_dependencies() {
        let mounts = mount_dependencies_from_properties(
            &[
                "mnt-gone.mount".to_string(),
                "mnt-primary.mount".to_string(),
            ],
            "Id=mnt-gone.mount\nWhere=\n\nId=mnt-primary.mount\nWhere=/mnt/primary\n",
        )
        .unwrap();

        assert_eq!(
            mounts,
            vec![systemd_mount("mnt-primary.mount", "/mnt/primary")]
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
    fn host_mount_points_are_read_from_systemctl_properties() {
        let mount_points = host_mount_points_from_properties(
            "Id=srv.mount\nWhere=/srv\n\nId=mnt-primary.mount\nWhere=/mnt/primary\n\nId=run-credentials-systemd\\x2dtmpfiles.service.mount\nWhere=\n",
        )
        .unwrap();

        assert_eq!(
            mount_points,
            BTreeSet::from([PathBuf::from("/srv"), PathBuf::from("/mnt/primary")])
        );
    }

    #[test]
    fn empty_host_mount_properties_fail_closed() {
        let error = nonempty_host_mount_points_from_properties("").unwrap_err();

        assert!(error.contains("did not report any host mount units"));
    }

    #[test]
    fn root_filesystem_distinguishes_unconfigured_and_expected_storage_paths() {
        let targets = vec![
            MountProtectionTarget {
                id: "systemd-mount-data-dir".to_string(),
                feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
                path: PathBuf::from("/var/lib/berrykeep"),
                mount_point: Some(PathBuf::from("/")),
                mount_point_is_bind: false,
                backing_mount_points: Vec::new(),
                path_resolution_error: None,
                missing_severity: HostDependencySeverity::Critical,
            },
            MountProtectionTarget {
                id: "systemd-mount-storage-data-child".to_string(),
                feature: "Systemd mount protection: storage pool `data-child` (active)".to_string(),
                path: PathBuf::from("/var/lib/berrykeep/pool-a"),
                mount_point: Some(PathBuf::from("/")),
                mount_point_is_bind: false,
                backing_mount_points: Vec::new(),
                path_resolution_error: None,
                missing_severity: HostDependencySeverity::Critical,
            },
            MountProtectionTarget {
                id: "systemd-mount-storage-primary".to_string(),
                feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
                path: PathBuf::from("/mnt/primary"),
                mount_point: Some(PathBuf::from("/")),
                mount_point_is_bind: false,
                backing_mount_points: Vec::new(),
                path_resolution_error: None,
                missing_severity: HostDependencySeverity::Critical,
            },
        ];
        let checks = checks_for_inspection(
            &targets,
            dependencies_with_host_mount_points(
                vec![systemd_mount("-.mount", "/")],
                &["/", "/mnt/primary"],
            ),
        );

        let root = checks
            .iter()
            .find(|check| check.id == "systemd-mount-data-dir")
            .unwrap();
        assert_eq!(root.status, HostDependencyStatus::NotApplicable);
        assert_eq!(root.severity, HostDependencySeverity::Info);
        assert!(root.install_hint.is_none());

        let data_child_storage = checks
            .iter()
            .find(|check| check.id == "systemd-mount-storage-data-child")
            .unwrap();
        assert_eq!(
            data_child_storage.status,
            HostDependencyStatus::NotApplicable
        );
        assert_eq!(data_child_storage.severity, HostDependencySeverity::Info);

        let root_storage = checks
            .iter()
            .find(|check| check.id == "systemd-mount-storage-primary")
            .unwrap();
        assert_eq!(root_storage.status, HostDependencyStatus::Missing);
        assert_eq!(root_storage.severity, HostDependencySeverity::Critical);
        assert!(root_storage.summary.contains("root filesystem"));
        assert!(
            root_storage
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("Mount the filesystem declared")
        );
    }

    #[test]
    fn unresolved_path_is_informational_and_not_treated_as_root_backed() {
        let target = MountProtectionTarget {
            id: "systemd-mount-storage-primary".to_string(),
            feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
            path: PathBuf::from("/srv/pool"),
            mount_point: Some(PathBuf::from("/")),
            mount_point_is_bind: false,
            backing_mount_points: Vec::new(),
            path_resolution_error: Some("readlink could not resolve the path".to_string()),
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(vec![systemd_mount("-.mount", "/")], &["/"]),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
        assert!(checks[0].summary.contains("Could not resolve"));
        assert!(
            checks[0]
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("Retry once")
        );
    }

    #[test]
    fn nested_storage_mount_requires_its_exact_mount_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-storage-primary".to_string(),
            feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
            path: PathBuf::from("/srv/pool/media"),
            mount_point: Some(PathBuf::from("/srv/pool")),
            mount_point_is_bind: false,
            backing_mount_points: Vec::new(),
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            std::slice::from_ref(&target),
            dependencies_with_host_mount_points(
                vec![systemd_mount("srv.mount", "/srv")],
                &["/srv", "/srv/pool"],
            ),
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Critical);

        let checks = checks_for_inspection(
            &[target],
            dependencies(vec![systemd_mount("srv-pool.mount", "/srv/pool")]),
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Ready);
    }

    #[test]
    fn expected_deeper_mount_cannot_use_an_ancestor_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-data-dir".to_string(),
            feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
            path: PathBuf::from("/srv/berrykeep"),
            mount_point: Some(PathBuf::from("/srv")),
            mount_point_is_bind: false,
            backing_mount_points: Vec::new(),
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(
                vec![systemd_mount("srv.mount", "/srv")],
                &["/srv", "/srv/berrykeep"],
            ),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Critical);
        assert!(checks[0].summary.contains("instead of"));
        assert_eq!(checks[0].resolved_path.as_deref(), Some("/srv"));
        assert!(
            checks[0]
                .install_hint
                .as_deref()
                .unwrap_or_default()
                .contains("Restore the filesystem declared")
        );
    }

    #[test]
    fn namespace_bind_mount_uses_its_mapped_source_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-data-dir".to_string(),
            feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
            path: PathBuf::from("/srv/berrykeep"),
            mount_point: Some(PathBuf::from("/srv/berrykeep")),
            mount_point_is_bind: true,
            backing_mount_points: vec![PathBuf::from("/mnt/data")],
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            std::slice::from_ref(&target),
            dependencies_with_host_mount_points(
                vec![systemd_mount("mnt-data.mount", "/mnt/data")],
                &["/mnt/data"],
            ),
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Ready);

        let unverified_target = MountProtectionTarget {
            backing_mount_points: Vec::new(),
            ..target
        };
        let checks = checks_for_inspection(
            &[unverified_target],
            dependencies_with_host_mount_points(
                vec![systemd_mount("srv.mount", "/srv")],
                &["/srv"],
            ),
        );
        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
    }

    #[test]
    fn host_bind_mount_cannot_use_its_backing_source_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-data-dir".to_string(),
            feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
            path: PathBuf::from("/srv/berrykeep"),
            mount_point: Some(PathBuf::from("/srv/berrykeep")),
            mount_point_is_bind: true,
            backing_mount_points: vec![PathBuf::from("/mnt/data")],
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(
                vec![systemd_mount("mnt-data.mount", "/mnt/data")],
                &["/srv/berrykeep", "/mnt/data"],
            ),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Critical);
    }

    #[test]
    fn namespace_bind_mount_can_use_its_host_ancestor_dependency() {
        let target = MountProtectionTarget {
            id: "systemd-mount-data-dir".to_string(),
            feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
            path: PathBuf::from("/srv/berrykeep"),
            mount_point: Some(PathBuf::from("/srv/berrykeep")),
            mount_point_is_bind: true,
            backing_mount_points: vec![PathBuf::from("/srv")],
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(
                vec![systemd_mount("srv.mount", "/srv")],
                &["/srv"],
            ),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::Ready);
    }

    #[test]
    fn mount_protection_paths_are_normalized_without_filesystem_access() {
        let path = normalized_mount_protection_path(Path::new("./data/server-node/../pool"));

        assert!(path.is_absolute());
        assert!(path.ends_with("data/pool"));
    }

    #[test]
    fn readlink_output_preserves_whitespace_in_path_components() {
        let path = readlink_output_path(b"/mnt/ primary \n").unwrap();
        assert_eq!(path, PathBuf::from("/mnt/ primary "));

        let path = readlink_output_path(b"/mnt/data \n\n").unwrap();
        assert_eq!(path, PathBuf::from("/mnt/data \n"));
    }

    #[test]
    fn non_utf8_readlink_output_fails_closed() {
        let error = readlink_output_path(b"/mnt/\xff\n").unwrap_err();

        assert_eq!(error, "returned a non-UTF-8 path");
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn resolved_mount_protection_path_follows_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "ironmesh-mount-protection-symlink-test-{}",
            std::process::id()
        ));
        let target = root.join("mounted-storage");
        let link = root.join("storage-link");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let canonicalizer = usable_path_canonicalizer().await.unwrap();
        let resolved = resolve_mount_protection_path(&link, &canonicalizer).await;

        assert_eq!(resolved.path, target);
        assert!(resolved.error.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn unresolved_mount_protection_path_reports_its_readlink_error() {
        let path = std::env::temp_dir().join(format!(
            "ironmesh-mount-protection-missing-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);

        let canonicalizer = usable_path_canonicalizer().await.unwrap();
        let resolved = resolve_mount_protection_path(&path, &canonicalizer).await;

        assert_eq!(resolved.path, path);
        assert!(resolved.error.is_some());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mountinfo_parser_reads_mount_points_and_roots() {
        let mount_points = mount_points_from_mountinfo(
            "36 25 0:32 /srv/berrykeep /srv/berrykeep rw,nosuid,nodev - ext4 /dev/sda1 rw\n",
        );

        assert_eq!(
            mount_points,
            vec![MountPoint {
                device: "0:32".to_string(),
                path: PathBuf::from("/srv/berrykeep"),
                root: PathBuf::from("/srv/berrykeep"),
            }]
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn bind_mount_source_is_mapped_to_its_backing_mount() {
        let mount_points = mount_points_from_mountinfo(
            "36 25 8:1 / /mnt/data rw,nosuid,nodev - ext4 /dev/sda1 rw\n37 25 8:1 /berrykeep /srv/berrykeep rw,nosuid,nodev - ext4 /dev/sda1 rw\n",
        );
        let mount_point =
            mount_point_for_path(Path::new("/srv/berrykeep/pool"), &mount_points).unwrap();

        assert_eq!(
            backing_mount_points_for_path(
                Path::new("/srv/berrykeep/pool"),
                mount_point,
                &mount_points,
            ),
            vec![PathBuf::from("/mnt/data")]
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn root_backed_namespace_bind_is_not_reported_as_an_unprotected_mount() {
        let data_dir = Path::new("/var/lib/berrykeep-server-node");
        let mount_points = mount_points_from_mountinfo(
            "36 25 8:1 / / rw,nosuid,nodev - ext4 /dev/sda1 rw\n37 25 8:1 /var/lib/berrykeep-server-node /var/lib/berrykeep-server-node rw,nosuid,nodev - ext4 /dev/sda1 rw\n",
        );
        let mount_point = mount_point_for_path(data_dir, &mount_points).unwrap();
        let target = MountProtectionTarget {
            id: "systemd-mount-data-dir".to_string(),
            feature: "Systemd mount protection: BERRYKEEP_DATA_DIR".to_string(),
            path: data_dir.to_path_buf(),
            mount_point: Some(mount_point.path.clone()),
            mount_point_is_bind: true,
            backing_mount_points: backing_mount_points_for_path(
                data_dir,
                mount_point,
                &mount_points,
            ),
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(Vec::new(), &["/"]),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::NotApplicable);
        assert_eq!(checks[0].severity, HostDependencySeverity::Info);
    }

    #[test]
    fn backing_mount_sources_are_reported_most_specific_first() {
        let mount_points = vec![
            MountPoint {
                device: "8:1".to_string(),
                path: PathBuf::from("/mnt/data"),
                root: PathBuf::from("/data"),
            },
            MountPoint {
                device: "8:1".to_string(),
                path: PathBuf::from("/mnt/data/nested"),
                root: PathBuf::from("/data/nested"),
            },
            MountPoint {
                device: "8:1".to_string(),
                path: PathBuf::from("/srv/berrykeep"),
                root: PathBuf::from("/data/nested"),
            },
        ];
        let mount_point =
            mount_point_for_path(Path::new("/srv/berrykeep/pool"), &mount_points).unwrap();

        assert_eq!(
            backing_mount_points_for_path(
                Path::new("/srv/berrykeep/pool"),
                mount_point,
                &mount_points,
            ),
            vec![
                PathBuf::from("/mnt/data/nested"),
                PathBuf::from("/mnt/data")
            ]
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn subvolume_mount_cannot_use_root_as_a_backing_dependency() {
        let mount_points = mount_points_from_mountinfo(
            "36 25 0:30 / / rw,nosuid,nodev - btrfs /dev/sda2 rw\n37 25 0:30 /data /mnt/data rw,nosuid,nodev - btrfs /dev/sda2 rw\n",
        );
        let mount_point = mount_point_for_path(Path::new("/mnt/data/pool"), &mount_points).unwrap();
        let target = MountProtectionTarget {
            id: "systemd-mount-storage-primary".to_string(),
            feature: "Systemd mount protection: storage pool `primary` (active)".to_string(),
            path: PathBuf::from("/mnt/data/pool"),
            mount_point: Some(mount_point.path.clone()),
            mount_point_is_bind: true,
            backing_mount_points: backing_mount_points_for_path(
                Path::new("/mnt/data/pool"),
                mount_point,
                &mount_points,
            ),
            path_resolution_error: None,
            missing_severity: HostDependencySeverity::Critical,
        };
        let checks = checks_for_inspection(
            &[target],
            dependencies_with_host_mount_points(
                vec![systemd_mount("-.mount", "/")],
                &["/", "/mnt/data"],
            ),
        );

        assert_eq!(checks[0].status, HostDependencyStatus::Missing);
        assert_eq!(checks[0].severity, HostDependencySeverity::Critical);
    }
}
