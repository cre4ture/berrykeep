use super::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::Path;
use std::process::Stdio;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

const NATURAL_EARTH_PHYSICAL_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/physical/10m_physical.zip";
const NATURAL_EARTH_COUNTRIES_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/cultural/ne_10m_admin_0_countries.zip";
const NATURAL_EARTH_POPULATED_PLACES_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/cultural/ne_10m_populated_places.zip";
const NATURAL_EARTH_BOUNDARIES_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/cultural/ne_10m_admin_0_boundary_lines_land.zip";
const NATURAL_EARTH_CROSS_BLENDED_HYPSO_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/raster/HYP_HR_SR_W.zip";
const NATURAL_EARTH_ONE_SHADED_RELIEF_WATER_10M_URL: &str =
    "https://naciscdn.org/naturalearth/10m/raster/NE1_HR_LC_SR_W.zip";
const NATURAL_EARTH_IMPORT_MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;
const NATURAL_EARTH_IMPORT_MAX_ARTIFACT_BYTES: usize = 512 * 1024 * 1024;
const NATURAL_EARTH_IMPORT_PART_BYTES: usize = 256 * 1024 * 1024;
const NATURAL_EARTH_COMMAND_TIMEOUT_SECS: u64 = 20 * 60;
const NATURAL_EARTH_COMMAND_OUTPUT_LIMIT: usize = 8 * 1024;
const NATURAL_EARTH_COMMAND_LOG_OUTPUT_LIMIT: usize = 2 * 1024;
const NATURAL_EARTH_IMPORT_LOG_MAX_ENTRIES: usize = 64;
const WEB_MERCATOR_WORLD_METERS: &str = "20037508.342789244";
const REQUIRED_PHYSICAL_IMPORT_COMMANDS: [(&str, &str); 5] = [
    ("unzip", "-v"),
    ("gdal_rasterize", "--version"),
    ("gdalwarp", "--version"),
    ("gdal_translate", "--version"),
    ("gdaladdo", "--version"),
];
const REQUIRED_CROSS_BLENDED_HYPSO_IMPORT_COMMANDS: [(&str, &str); 4] = [
    ("unzip", "-v"),
    ("gdalwarp", "--version"),
    ("gdal_translate", "--version"),
    ("gdaladdo", "--version"),
];
const REQUIRED_LABEL_IMPORT_COMMANDS: [(&str, &str); 1] = [("ogr2ogr", "--version")];
const REQUIRED_VECTOR_IMPORT_COMMANDS: [(&str, &str); 2] =
    [("unzip", "-v"), ("ogr2ogr", "--version")];
const NATURAL_EARTH_LABEL_MAX_ZOOM: &str = "8";
const NATURAL_EARTH_VECTOR_MAX_ZOOM: &str = "6";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NaturalEarthImportProfile {
    #[default]
    Physical,
    PhysicalWithLabels,
    PhysicalVector,
    CrossBlendedHypso,
    NaturalEarthOne,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StartNaturalEarthImportRequest {
    #[serde(default)]
    profile: NaturalEarthImportProfile,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NaturalEarthImportState {
    Running,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NaturalEarthImportLogEntry {
    pub(crate) timestamp_unix: u64,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NaturalEarthImportArtifactView {
    pub(crate) variant_id: String,
    pub(crate) asset: map_config::MapVariantAssetKind,
    pub(crate) logical_key: String,
    pub(crate) manifest_key: String,
    pub(crate) logical_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NaturalEarthImportJobView {
    pub(crate) id: String,
    pub(crate) state: NaturalEarthImportState,
    pub(crate) profile: NaturalEarthImportProfile,
    pub(crate) phase: String,
    pub(crate) source_url: String,
    pub(crate) logical_key: String,
    pub(crate) manifest_key: String,
    pub(crate) logical_size_bytes: u64,
    pub(crate) artifacts: Vec<NaturalEarthImportArtifactView>,
    pub(crate) error: Option<String>,
    pub(crate) log_entries: Vec<NaturalEarthImportLogEntry>,
    pub(crate) started_at_unix: u64,
    pub(crate) updated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NaturalEarthImportStatusResponse {
    pub(crate) active_job: Option<NaturalEarthImportJobView>,
    pub(crate) can_start_new: bool,
}

#[derive(Debug, Default)]
pub(crate) struct NaturalEarthImportRuntime {
    job: Option<NaturalEarthImportJobView>,
}

#[derive(Debug, Serialize)]
struct SplitFileManifestDocument {
    manifest_version: u32,
    #[serde(rename = "type")]
    manifest_type: &'static str,
    logical_format: &'static str,
    logical_key: String,
    manifest_key: String,
    storage_root: &'static str,
    logical_size_bytes: u64,
    last_part_size_bytes: u64,
    parts_count: usize,
    parts: Vec<SplitFileManifestPartDocument>,
}

#[derive(Debug, Serialize)]
struct SplitFileManifestPartDocument {
    part_id: String,
    key: String,
    offset_bytes: u64,
    size_bytes: u64,
}

struct PhysicalMapLayers<'a> {
    ocean: &'a Path,
    land: &'a Path,
    lakes: &'a Path,
    rivers: &'a Path,
    coastline: &'a Path,
}

struct NaturalEarthLabelLayers<'a> {
    countries: &'a Path,
    populated_places: &'a Path,
    boundaries: &'a Path,
}

struct NaturalEarthLabelLayerPaths {
    countries: PathBuf,
    populated_places: PathBuf,
    boundaries: PathBuf,
}

struct NaturalEarthSourceArchive {
    url: &'static str,
    archive_name: &'static str,
    description: &'static str,
}

struct RasterMbtilesConversion<'a> {
    name: &'a str,
    description: &'a str,
    resampling: &'a str,
    add_destination_alpha: bool,
}

pub(crate) async fn is_running(state: &ServerState) -> bool {
    let runtime = state.storage.natural_earth_import.lock().await;
    matches!(
        runtime.job.as_ref().map(|job| &job.state),
        Some(NaturalEarthImportState::Running)
    )
}

pub(crate) async fn admin_status(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/maps/import/natural-earth/status",
        true,
        true,
        json!({}),
    )
    .await
    {
        return status.into_response();
    }
    let runtime = state.storage.natural_earth_import.lock().await;
    Json(NaturalEarthImportStatusResponse {
        active_job: runtime.job.clone(),
        can_start_new: !matches!(
            runtime.job.as_ref().map(|job| &job.state),
            Some(NaturalEarthImportState::Running)
        ),
    })
    .into_response()
}

pub(crate) async fn start_admin_import(
    State(state): State<ServerState>,
    headers: HeaderMap,
    request: Option<Json<StartNaturalEarthImportRequest>>,
) -> Response {
    let action = "auth/maps/import/natural-earth/run";
    let profile = request
        .map(|Json(request)| request.profile)
        .unwrap_or_default();
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        true,
        true,
        json!({ "profile": profile }),
    )
    .await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };

    let response = match start_import(&state, profile).await {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err((status, error)) => {
            (status, Json(json!({ "error": error.to_string() }))).into_response()
        }
    };
    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        if response.status().is_success() {
            "started"
        } else {
            "error"
        },
        json!({ "status": response.status().as_u16() }),
    )
    .await;
    response
}

async fn start_import(
    state: &ServerState,
    profile: NaturalEarthImportProfile,
) -> std::result::Result<NaturalEarthImportJobView, (StatusCode, anyhow::Error)> {
    start_import_with_worker(state, profile, |worker_state, worker_job| async move {
        run_import(&worker_state, &worker_job).await
    })
    .await
}

async fn start_import_with_worker<Worker, WorkerFuture>(
    state: &ServerState,
    profile: NaturalEarthImportProfile,
    worker: Worker,
) -> std::result::Result<NaturalEarthImportJobView, (StatusCode, anyhow::Error)>
where
    Worker: FnOnce(ServerState, NaturalEarthImportJobView) -> WorkerFuture + Send + 'static,
    WorkerFuture: std::future::Future<Output = Result<u64>> + Send + 'static,
{
    if map_dataset_import::has_running_job(state).await {
        return Err((
            StatusCode::CONFLICT,
            anyhow!("another map dataset import is already running"),
        ));
    }
    let configuration = map_config::load_current_configuration(state)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let (base_variant_id, primary_asset) = match profile {
        NaturalEarthImportProfile::Physical => (
            "natural-earth-globe",
            map_config::MapVariantAssetKind::Raster,
        ),
        NaturalEarthImportProfile::PhysicalWithLabels => (
            "natural-earth-labels",
            map_config::MapVariantAssetKind::Raster,
        ),
        NaturalEarthImportProfile::PhysicalVector => (
            "natural-earth-vector",
            map_config::MapVariantAssetKind::Vector,
        ),
        NaturalEarthImportProfile::CrossBlendedHypso => (
            "natural-earth-hypso",
            map_config::MapVariantAssetKind::Raster,
        ),
        NaturalEarthImportProfile::NaturalEarthOne => {
            ("natural-earth-1", map_config::MapVariantAssetKind::Raster)
        }
    };
    let target = map_config::resolve_import_target(
        &configuration.configuration,
        base_variant_id,
        primary_asset,
    )
    .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let mut artifacts = vec![NaturalEarthImportArtifactView {
        variant_id: target.variant_id.clone(),
        asset: target.asset,
        logical_key: target.logical_key.clone(),
        manifest_key: target.manifest_key.clone(),
        logical_size_bytes: 0,
    }];
    if profile == NaturalEarthImportProfile::PhysicalWithLabels {
        let label_target = map_config::resolve_import_target(
            &configuration.configuration,
            "natural-earth-labels",
            map_config::MapVariantAssetKind::Vector,
        )
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        artifacts.push(NaturalEarthImportArtifactView {
            variant_id: label_target.variant_id,
            asset: label_target.asset,
            logical_key: label_target.logical_key,
            manifest_key: label_target.manifest_key,
            logical_size_bytes: 0,
        });
    }
    let now = unix_ts();
    let job = NaturalEarthImportJobView {
        id: Uuid::now_v7().to_string(),
        state: NaturalEarthImportState::Running,
        profile,
        phase: "Waiting for the Natural Earth conversion worker".to_string(),
        source_url: natural_earth_source_url(profile).to_string(),
        logical_key: target.logical_key,
        manifest_key: target.manifest_key,
        logical_size_bytes: 0,
        artifacts,
        error: None,
        log_entries: vec![NaturalEarthImportLogEntry {
            timestamp_unix: now,
            message: "Import job accepted; waiting for the conversion worker".to_string(),
        }],
        started_at_unix: now,
        updated_at_unix: now,
    };
    {
        let mut runtime = state.storage.natural_earth_import.lock().await;
        if matches!(
            runtime.job.as_ref().map(|existing| &existing.state),
            Some(NaturalEarthImportState::Running)
        ) {
            return Err((
                StatusCode::CONFLICT,
                anyhow!("a Natural Earth conversion is already running"),
            ));
        }
        runtime.job = Some(job.clone());
    }
    let worker_state = state.clone();
    let worker_job = job.clone();
    tokio::spawn(async move {
        let result = worker(worker_state.clone(), worker_job.clone()).await;
        match result {
            Ok(size) => {
                finish_job(
                    &worker_state,
                    NaturalEarthImportState::Ready,
                    "Ready",
                    size,
                    None,
                )
                .await
            }
            Err(error) => {
                warn!(natural_earth_import_id = %worker_job.id, error = %error, "Natural Earth map import failed");
                finish_job(
                    &worker_state,
                    NaturalEarthImportState::Failed,
                    "Failed",
                    0,
                    Some(error.to_string()),
                )
                .await;
            }
        }
    });
    Ok(job)
}

fn natural_earth_source_url(profile: NaturalEarthImportProfile) -> &'static str {
    match profile {
        NaturalEarthImportProfile::Physical
        | NaturalEarthImportProfile::PhysicalWithLabels
        | NaturalEarthImportProfile::PhysicalVector => NATURAL_EARTH_PHYSICAL_10M_URL,
        NaturalEarthImportProfile::CrossBlendedHypso => NATURAL_EARTH_CROSS_BLENDED_HYPSO_10M_URL,
        NaturalEarthImportProfile::NaturalEarthOne => NATURAL_EARTH_ONE_SHADED_RELIEF_WATER_10M_URL,
    }
}

async fn run_import(state: &ServerState, job: &NaturalEarthImportJobView) -> Result<u64> {
    update_phase(state, "Checking GDAL and unzip dependencies").await;
    ensure_import_commands_available(state, job.profile).await?;
    let staging_dir = state
        .managed_paths
        .data_dir()
        .join("state")
        .join("natural-earth-imports")
        .join(&job.id);
    fs::create_dir_all(&staging_dir).await.with_context(|| {
        format!(
            "failed creating Natural Earth staging directory {}",
            staging_dir.display()
        )
    })?;

    let result = async {
        let (raster_artifact_path, vector_artifact_path) = match job.profile {
            NaturalEarthImportProfile::CrossBlendedHypso => (
                Some(create_cross_blended_hypso_mbtiles(state, &staging_dir).await?),
                None,
            ),
            NaturalEarthImportProfile::NaturalEarthOne => (
                Some(create_natural_earth_one_mbtiles(state, &staging_dir).await?),
                None,
            ),
            NaturalEarthImportProfile::Physical | NaturalEarthImportProfile::PhysicalWithLabels => {
                let (raster_path, label_vector_path) =
                    create_physical_map_mbtiles(state, job.profile, &staging_dir).await?;
                (Some(raster_path), label_vector_path)
            }
            NaturalEarthImportProfile::PhysicalVector => (
                None,
                Some(create_natural_earth_vector_mbtiles(state, &staging_dir).await?),
            ),
        };

        let raster_size = if let Some(raster_artifact_path) = raster_artifact_path {
            update_phase(state, "Publishing generated raster map artifact").await;
            let raster_artifact =
                required_job_artifact(job, map_config::MapVariantAssetKind::Raster)?;
            let size =
                publish_generated_mbtiles(state, job, raster_artifact, &raster_artifact_path)
                    .await?;
            record_published_artifact(state, &raster_artifact.manifest_key, size).await;
            size
        } else {
            0
        };

        let vector_size = if let Some(vector_artifact_path) = vector_artifact_path {
            update_phase(state, "Publishing generated vector map artifact").await;
            let vector_artifact =
                required_job_artifact(job, map_config::MapVariantAssetKind::Vector)?;
            let size =
                publish_generated_mbtiles(state, job, vector_artifact, &vector_artifact_path)
                    .await?;
            record_published_artifact(state, &vector_artifact.manifest_key, size).await;
            size
        } else {
            0
        };
        raster_size
            .checked_add(vector_size)
            .context("published Natural Earth artifact size overflow")
    }
    .await;
    if let Err(error) = fs::remove_dir_all(&staging_dir).await {
        warn!(path = %staging_dir.display(), error = %error, "failed cleaning Natural Earth import staging directory");
    }
    result
}

async fn create_cross_blended_hypso_mbtiles(
    state: &ServerState,
    staging_dir: &Path,
) -> Result<PathBuf> {
    let extracted_dir = staging_dir.join("source");
    fs::create_dir_all(&extracted_dir).await?;
    download_and_extract_natural_earth_archive(
        state,
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_CROSS_BLENDED_HYPSO_10M_URL,
            archive_name: "natural-earth-cross-blended-hypso.zip",
            description: "Natural Earth cross-blended hypsometric relief archive",
        },
        &extracted_dir,
        staging_dir,
    )
    .await?;
    let source_raster = find_layer(&extracted_dir, "HYP_HR_SR_W.tif")?;
    let mercator_raster = staging_dir.join("natural-earth-hypso-mercator.tif");
    let artifact_path = staging_dir.join("natural-earth-hypso.mbtiles");
    update_phase(state, "Rendering the cross-blended hypsometric relief map").await;
    project_and_create_mbtiles(
        state,
        &source_raster,
        &mercator_raster,
        &artifact_path,
        staging_dir,
        RasterMbtilesConversion {
            name: "Natural Earth Hypsometric Relief",
            description: "Natural Earth 10m cross-blended hypsometric tints with shaded relief and water",
            resampling: "bilinear",
            add_destination_alpha: true,
        },
    )
    .await?;
    update_phase(state, "Validating generated relief MBTiles").await;
    validate_generated_mbtiles(state, &artifact_path).await?;
    Ok(artifact_path)
}

async fn create_natural_earth_one_mbtiles(
    state: &ServerState,
    staging_dir: &Path,
) -> Result<PathBuf> {
    let extracted_dir = staging_dir.join("source");
    fs::create_dir_all(&extracted_dir).await?;
    download_and_extract_natural_earth_archive(
        state,
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_ONE_SHADED_RELIEF_WATER_10M_URL,
            archive_name: "natural-earth-one-shaded-relief-water.zip",
            description: "Natural Earth I shaded-relief and water archive",
        },
        &extracted_dir,
        staging_dir,
    )
    .await?;
    let source_raster = find_layer(&extracted_dir, "NE1_HR_LC_SR_W.tif")?;
    let mercator_raster = staging_dir.join("natural-earth-one-mercator.tif");
    let artifact_path = staging_dir.join("natural-earth-one.mbtiles");
    update_phase(
        state,
        "Rendering the Natural Earth I shaded-relief and water map",
    )
    .await;
    project_and_create_mbtiles(
        state,
        &source_raster,
        &mercator_raster,
        &artifact_path,
        staging_dir,
        RasterMbtilesConversion {
            name: "Natural Earth I with Shaded Relief and Water",
            description: "Natural Earth I 10m land cover with shaded relief and water",
            resampling: "bilinear",
            add_destination_alpha: true,
        },
    )
    .await?;
    update_phase(state, "Validating generated Natural Earth I MBTiles").await;
    validate_generated_mbtiles(state, &artifact_path).await?;
    Ok(artifact_path)
}

async fn create_physical_map_mbtiles(
    state: &ServerState,
    profile: NaturalEarthImportProfile,
    staging_dir: &Path,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let extracted_dir = staging_dir.join("source");
    fs::create_dir_all(&extracted_dir).await?;
    download_and_extract_natural_earth_archive(
        state,
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_PHYSICAL_10M_URL,
            archive_name: "natural-earth-physical.zip",
            description: "Natural Earth physical archive",
        },
        &extracted_dir,
        staging_dir,
    )
    .await?;
    let ocean = find_layer(&extracted_dir, "ne_10m_ocean.shp")?;
    let land = find_layer(&extracted_dir, "ne_10m_land.shp")?;
    let lakes = find_layer(&extracted_dir, "ne_10m_lakes.shp")?;
    let rivers = find_layer(&extracted_dir, "ne_10m_rivers_lake_centerlines.shp")?;
    let coastline = find_layer(&extracted_dir, "ne_10m_coastline.shp")?;
    let source_raster = staging_dir.join("natural-earth-physical-wgs84.tif");
    let mercator_raster = staging_dir.join("natural-earth-physical-mercator.tif");
    let artifact_path = staging_dir.join("natural-earth-globe.mbtiles");
    update_phase(state, "Rendering the physical world map").await;
    rasterize_physical_map(
        state,
        PhysicalMapLayers {
            ocean: &ocean,
            land: &land,
            lakes: &lakes,
            rivers: &rivers,
            coastline: &coastline,
        },
        &source_raster,
        staging_dir,
    )
    .await?;
    update_phase(state, "Generating Web Mercator MBTiles").await;
    project_and_create_mbtiles(
        state,
        &source_raster,
        &mercator_raster,
        &artifact_path,
        staging_dir,
        RasterMbtilesConversion {
            name: "Natural Earth Globe",
            description: "Natural Earth Physical 10m",
            resampling: "near",
            add_destination_alpha: false,
        },
    )
    .await?;
    update_phase(state, "Validating generated MBTiles").await;
    validate_generated_mbtiles(state, &artifact_path).await?;

    let label_artifact_path = if profile == NaturalEarthImportProfile::PhysicalWithLabels {
        Some(create_natural_earth_label_mbtiles(state, staging_dir).await?)
    } else {
        None
    };
    Ok((artifact_path, label_artifact_path))
}

async fn create_natural_earth_label_mbtiles(
    state: &ServerState,
    staging_dir: &Path,
) -> Result<PathBuf> {
    let layers = download_natural_earth_label_layers(state, staging_dir).await?;
    let label_geopackage = staging_dir.join("natural-earth-labels.gpkg");
    let label_mbtiles = staging_dir.join("natural-earth-labels.mbtiles");
    update_phase(state, "Preparing Natural Earth country and place labels").await;
    create_natural_earth_label_overlay(
        state,
        NaturalEarthLabelLayers {
            countries: &layers.countries,
            populated_places: &layers.populated_places,
            boundaries: &layers.boundaries,
        },
        &label_geopackage,
        &label_mbtiles,
        staging_dir,
    )
    .await?;
    update_phase(state, "Validating generated label overlay").await;
    validate_generated_label_mbtiles(state, &label_mbtiles).await?;
    Ok(label_mbtiles)
}

async fn create_natural_earth_vector_mbtiles(
    state: &ServerState,
    staging_dir: &Path,
) -> Result<PathBuf> {
    let physical_source_dir = staging_dir.join("physical-source");
    fs::create_dir_all(&physical_source_dir).await?;
    download_and_extract_natural_earth_archive(
        state,
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_PHYSICAL_10M_URL,
            archive_name: "natural-earth-physical.zip",
            description: "Natural Earth physical archive",
        },
        &physical_source_dir,
        staging_dir,
    )
    .await?;
    let ocean = find_layer(&physical_source_dir, "ne_10m_ocean.shp")?;
    let land = find_layer(&physical_source_dir, "ne_10m_land.shp")?;
    let lakes = find_layer(&physical_source_dir, "ne_10m_lakes.shp")?;
    let rivers = find_layer(&physical_source_dir, "ne_10m_rivers_lake_centerlines.shp")?;
    let coastline = find_layer(&physical_source_dir, "ne_10m_coastline.shp")?;
    let label_layers = download_natural_earth_label_layers(state, staging_dir).await?;
    let geopackage = staging_dir.join("natural-earth-vector.gpkg");
    let artifact_path = staging_dir.join("natural-earth-vector.mbtiles");
    update_phase(state, "Preparing Natural Earth physical vector layers").await;
    create_natural_earth_vector_tiles(
        state,
        PhysicalMapLayers {
            ocean: &ocean,
            land: &land,
            lakes: &lakes,
            rivers: &rivers,
            coastline: &coastline,
        },
        NaturalEarthLabelLayers {
            countries: &label_layers.countries,
            populated_places: &label_layers.populated_places,
            boundaries: &label_layers.boundaries,
        },
        &geopackage,
        &artifact_path,
        staging_dir,
    )
    .await?;
    update_phase(state, "Validating generated Natural Earth vector map").await;
    validate_generated_vector_mbtiles(
        state,
        &artifact_path,
        "Natural Earth vector map",
        &[
            "ne_land",
            "ne_ocean",
            "ne_lakes",
            "ne_rivers",
            "ne_coastline",
            "ne_places",
            "ne_boundaries",
        ],
    )
    .await?;
    Ok(artifact_path)
}

async fn download_natural_earth_label_layers(
    state: &ServerState,
    staging_dir: &Path,
) -> Result<NaturalEarthLabelLayerPaths> {
    let labels_source_dir = staging_dir.join("labels-source");
    fs::create_dir_all(&labels_source_dir).await?;
    for archive in [
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_COUNTRIES_10M_URL,
            archive_name: "natural-earth-countries.zip",
            description: "Natural Earth countries archive",
        },
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_POPULATED_PLACES_10M_URL,
            archive_name: "natural-earth-populated-places.zip",
            description: "Natural Earth populated places archive",
        },
        NaturalEarthSourceArchive {
            url: NATURAL_EARTH_BOUNDARIES_10M_URL,
            archive_name: "natural-earth-boundaries.zip",
            description: "Natural Earth country boundaries archive",
        },
    ] {
        download_and_extract_natural_earth_archive(state, archive, &labels_source_dir, staging_dir)
            .await?;
    }
    Ok(NaturalEarthLabelLayerPaths {
        countries: find_layer(&labels_source_dir, "ne_10m_admin_0_countries.shp")?,
        populated_places: find_layer(&labels_source_dir, "ne_10m_populated_places.shp")?,
        boundaries: find_layer(&labels_source_dir, "ne_10m_admin_0_boundary_lines_land.shp")?,
    })
}

async fn update_phase(state: &ServerState, phase: &str) {
    let mut runtime = state.storage.natural_earth_import.lock().await;
    if let Some(job) = runtime.job.as_mut()
        && matches!(job.state, NaturalEarthImportState::Running)
    {
        job.phase = phase.to_string();
        job.updated_at_unix = unix_ts();
        append_job_log(job, format!("Phase: {phase}"));
    }
}

async fn append_import_log(state: &ServerState, message: impl Into<String>) {
    let mut runtime = state.storage.natural_earth_import.lock().await;
    if let Some(job) = runtime.job.as_mut() {
        append_job_log(job, message);
        job.updated_at_unix = unix_ts();
    }
}

fn append_job_log(job: &mut NaturalEarthImportJobView, message: impl Into<String>) {
    if job.log_entries.len() == NATURAL_EARTH_IMPORT_LOG_MAX_ENTRIES {
        job.log_entries.remove(0);
    }
    job.log_entries.push(NaturalEarthImportLogEntry {
        timestamp_unix: unix_ts(),
        message: message.into(),
    });
}

async fn finish_job(
    state: &ServerState,
    import_state: NaturalEarthImportState,
    phase: &str,
    logical_size_bytes: u64,
    error: Option<String>,
) {
    let mut runtime = state.storage.natural_earth_import.lock().await;
    if let Some(job) = runtime.job.as_mut() {
        job.phase = phase.to_string();
        if !matches!(import_state, NaturalEarthImportState::Failed) || logical_size_bytes > 0 {
            job.logical_size_bytes = logical_size_bytes;
        }
        job.error = error;
        job.updated_at_unix = unix_ts();
        let completion_log = match &import_state {
            NaturalEarthImportState::Ready => Some("Import completed successfully".to_string()),
            NaturalEarthImportState::Failed => Some(format!(
                "Import failed: {}",
                job.error.as_deref().unwrap_or("unknown error")
            )),
            NaturalEarthImportState::Running => None,
        };
        job.state = import_state;
        if let Some(completion_log) = completion_log {
            append_job_log(job, completion_log);
        }
    }
}

async fn ensure_import_commands_available(
    state: &ServerState,
    profile: NaturalEarthImportProfile,
) -> Result<()> {
    for (command, version_argument) in required_import_commands(profile) {
        let arguments = vec![version_argument.into()];
        let command_display = format_command(command, &arguments);
        append_import_log(
            state,
            format!("Checking required command: {command_display}"),
        )
        .await;
        match timeout(
            Duration::from_secs(10),
            Command::new(command)
                .args(&arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        {
            Ok(Ok(output)) if output.status.success() => {
                append_import_log(
                    state,
                    format!("Required command is available: {command_display}"),
                )
                .await;
            }
            Ok(Ok(output)) => {
                let error = command_failure_message(&command_display, &output);
                append_import_log(state, format!("Required command check failed: {error}")).await;
                bail!("required map import command {error}")
            }
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                let error = format!("`{command_display}` is not installed or not on PATH");
                append_import_log(state, format!("Required command check failed: {error}")).await;
                bail!("required map import command {error}")
            }
            Ok(Err(error)) => {
                let error = format!("could not start `{command_display}`: {error}");
                append_import_log(state, format!("Required command check failed: {error}")).await;
                bail!("failed starting required map import command {error}")
            }
            Err(_) => {
                let error = format!("`{command_display}` timed out after 10 seconds");
                append_import_log(state, format!("Required command check failed: {error}")).await;
                bail!("required map import command {error}")
            }
        }
    }
    Ok(())
}

fn required_import_commands(
    profile: NaturalEarthImportProfile,
) -> Vec<(&'static str, &'static str)> {
    let mut commands = match profile {
        NaturalEarthImportProfile::CrossBlendedHypso => {
            REQUIRED_CROSS_BLENDED_HYPSO_IMPORT_COMMANDS.to_vec()
        }
        NaturalEarthImportProfile::NaturalEarthOne => {
            REQUIRED_CROSS_BLENDED_HYPSO_IMPORT_COMMANDS.to_vec()
        }
        NaturalEarthImportProfile::PhysicalVector => REQUIRED_VECTOR_IMPORT_COMMANDS.to_vec(),
        NaturalEarthImportProfile::Physical | NaturalEarthImportProfile::PhysicalWithLabels => {
            REQUIRED_PHYSICAL_IMPORT_COMMANDS.to_vec()
        }
    };
    if profile == NaturalEarthImportProfile::PhysicalWithLabels {
        commands.extend(REQUIRED_LABEL_IMPORT_COMMANDS);
    }
    commands
}

async fn download_and_extract_natural_earth_archive(
    state: &ServerState,
    archive: NaturalEarthSourceArchive,
    destination_dir: &Path,
    working_dir: &Path,
) -> Result<()> {
    let destination = working_dir.join(archive.archive_name);
    update_phase(state, &format!("Downloading {}", archive.description)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10 * 60))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .context("failed building Natural Earth download client")?;
    let mut response = client
        .get(archive.url)
        .send()
        .await
        .with_context(|| format!("failed downloading the {}", archive.description))?
        .error_for_status()
        .with_context(|| format!("{} request was rejected", archive.description))?;
    if response
        .content_length()
        .is_some_and(|size| size > NATURAL_EARTH_IMPORT_MAX_DOWNLOAD_BYTES as u64)
    {
        bail!(
            "{} exceeds the configured download size limit",
            archive.description
        )
    }
    let mut downloaded_bytes = 0usize;
    let mut destination_file = fs::File::create(&destination).await.with_context(|| {
        format!(
            "failed creating {} {}",
            archive.description,
            destination.display()
        )
    })?;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed reading Natural Earth physical archive response")?
    {
        let next_size = downloaded_bytes
            .checked_add(chunk.len())
            .with_context(|| format!("{} size overflow", archive.description))?;
        if next_size > NATURAL_EARTH_IMPORT_MAX_DOWNLOAD_BYTES {
            bail!(
                "{} exceeds the configured download size limit",
                archive.description
            )
        }
        destination_file.write_all(&chunk).await.with_context(|| {
            format!(
                "failed writing {} {}",
                archive.description,
                destination.display()
            )
        })?;
        downloaded_bytes = next_size;
    }
    destination_file.flush().await.with_context(|| {
        format!(
            "failed flushing {} {}",
            archive.description,
            destination.display()
        )
    })?;
    append_import_log(
        state,
        format!(
            "Downloaded {} ({downloaded_bytes} bytes)",
            archive.description
        ),
    )
    .await;
    update_phase(state, &format!("Extracting {}", archive.description)).await;
    run_command(
        state,
        "unzip",
        [
            "-q".into(),
            destination.as_os_str().to_owned(),
            "-d".into(),
            destination_dir.as_os_str().to_owned(),
        ],
        working_dir,
    )
    .await
}

async fn rasterize_physical_map(
    state: &ServerState,
    layers: PhysicalMapLayers<'_>,
    destination: &Path,
    working_dir: &Path,
) -> Result<()> {
    run_command(
        state,
        "gdal_rasterize",
        rasterize_create_arguments(layers.ocean, destination, [199, 224, 239, 255]),
        working_dir,
    )
    .await?;
    for (layer, color) in [
        (layers.land, [225, 232, 205, 255]),
        (layers.lakes, [171, 211, 233, 255]),
        (layers.rivers, [104, 170, 214, 255]),
        (layers.coastline, [94, 117, 122, 255]),
    ] {
        run_command(
            state,
            "gdal_rasterize",
            rasterize_overlay_arguments(layer, destination, color),
            working_dir,
        )
        .await?;
    }
    Ok(())
}

fn rasterize_create_arguments(
    source: &Path,
    destination: &Path,
    color: [u8; 4],
) -> Vec<std::ffi::OsString> {
    let mut args = vec![
        "-of".into(),
        "GTiff".into(),
        "-te".into(),
        "-180".into(),
        "-90".into(),
        "180".into(),
        "90".into(),
        "-ts".into(),
        "4096".into(),
        "2048".into(),
        "-a_srs".into(),
        "EPSG:4326".into(),
        "-ot".into(),
        "Byte".into(),
    ];
    append_raster_color_arguments(&mut args, color, false);
    args.push(source.as_os_str().to_owned());
    args.push(destination.as_os_str().to_owned());
    args
}

fn rasterize_overlay_arguments(
    source: &Path,
    destination: &Path,
    color: [u8; 4],
) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    append_raster_color_arguments(&mut args, color, true);
    args.push(source.as_os_str().to_owned());
    args.push(destination.as_os_str().to_owned());
    args
}

fn append_raster_color_arguments(
    args: &mut Vec<std::ffi::OsString>,
    color: [u8; 4],
    include_bands: bool,
) {
    for band in 1..=4 {
        if include_bands {
            args.push("-b".into());
            args.push(band.to_string().into());
        }
        args.push("-burn".into());
        args.push(color[(band - 1) as usize].to_string().into());
    }
}

async fn project_and_create_mbtiles(
    state: &ServerState,
    source: &Path,
    mercator: &Path,
    artifact: &Path,
    working_dir: &Path,
    conversion: RasterMbtilesConversion<'_>,
) -> Result<()> {
    let mut warp_arguments = vec![
        "-overwrite".into(),
        "-t_srs".into(),
        "EPSG:3857".into(),
        "-te".into(),
        format!("-{WEB_MERCATOR_WORLD_METERS}").into(),
        format!("-{WEB_MERCATOR_WORLD_METERS}").into(),
        WEB_MERCATOR_WORLD_METERS.into(),
        WEB_MERCATOR_WORLD_METERS.into(),
        "-ts".into(),
        "8192".into(),
        "8192".into(),
        "-r".into(),
        conversion.resampling.into(),
    ];
    if conversion.add_destination_alpha {
        warp_arguments.push("-dstalpha".into());
    }
    warp_arguments.push(source.as_os_str().to_owned());
    warp_arguments.push(mercator.as_os_str().to_owned());
    run_command(state, "gdalwarp", warp_arguments, working_dir).await?;
    run_command(
        state,
        "gdal_translate",
        vec![
            "-of".into(),
            "MBTILES".into(),
            "-b".into(),
            "1".into(),
            "-b".into(),
            "2".into(),
            "-b".into(),
            "3".into(),
            "-b".into(),
            "4".into(),
            "-colorinterp".into(),
            "red,green,blue,alpha".into(),
            "-co".into(),
            format!("NAME={}", conversion.name).into(),
            "-co".into(),
            format!("DESCRIPTION={}", conversion.description).into(),
            "-co".into(),
            "TYPE=baselayer".into(),
            "-co".into(),
            "TILE_FORMAT=PNG".into(),
            "-co".into(),
            "ZOOM_LEVEL_STRATEGY=UPPER".into(),
            mercator.as_os_str().to_owned(),
            artifact.as_os_str().to_owned(),
        ],
        working_dir,
    )
    .await?;
    run_command(
        state,
        "gdaladdo",
        vec![
            "-r".into(),
            "average".into(),
            artifact.as_os_str().to_owned(),
            "2".into(),
            "4".into(),
            "8".into(),
            "16".into(),
            "32".into(),
        ],
        working_dir,
    )
    .await
}

async fn create_natural_earth_label_overlay(
    state: &ServerState,
    layers: NaturalEarthLabelLayers<'_>,
    geopackage: &Path,
    artifact: &Path,
    working_dir: &Path,
) -> Result<()> {
    append_natural_earth_label_layers(state, layers, geopackage, working_dir).await?;
    run_command(
        state,
        "ogr2ogr",
        vector_mbtiles_arguments(
            geopackage,
            artifact,
            "Natural Earth labels",
            "Natural Earth countries, populated places, and borders",
            "overlay",
            NATURAL_EARTH_LABEL_MAX_ZOOM,
        ),
        working_dir,
    )
    .await
}

async fn create_natural_earth_vector_tiles(
    state: &ServerState,
    physical_layers: PhysicalMapLayers<'_>,
    label_layers: NaturalEarthLabelLayers<'_>,
    geopackage: &Path,
    artifact: &Path,
    working_dir: &Path,
) -> Result<()> {
    append_natural_earth_label_layers(state, label_layers, geopackage, working_dir).await?;
    append_natural_earth_physical_vector_layers(state, physical_layers, geopackage, working_dir)
        .await?;
    run_command(
        state,
        "ogr2ogr",
        vector_mbtiles_arguments(
            geopackage,
            artifact,
            "Natural Earth Vector",
            "Natural Earth physical world map with countries, populated places, and boundaries",
            "baselayer",
            NATURAL_EARTH_VECTOR_MAX_ZOOM,
        ),
        working_dir,
    )
    .await
}

async fn append_natural_earth_label_layers(
    state: &ServerState,
    layers: NaturalEarthLabelLayers<'_>,
    geopackage: &Path,
    working_dir: &Path,
) -> Result<()> {
    run_command(
        state,
        "ogr2ogr",
        country_label_geopackage_arguments(layers.countries, geopackage),
        working_dir,
    )
    .await?;
    run_command(
        state,
        "ogr2ogr",
        populated_place_label_geopackage_arguments(layers.populated_places, geopackage),
        working_dir,
    )
    .await?;
    run_command(
        state,
        "ogr2ogr",
        boundary_geopackage_arguments(layers.boundaries, geopackage),
        working_dir,
    )
    .await
}

async fn append_natural_earth_physical_vector_layers(
    state: &ServerState,
    layers: PhysicalMapLayers<'_>,
    geopackage: &Path,
    working_dir: &Path,
) -> Result<()> {
    for (source, layer_name, source_layer_name) in [
        (layers.ocean, "ne_ocean", "ne_10m_ocean"),
        (layers.land, "ne_land", "ne_10m_land"),
        (layers.lakes, "ne_lakes", "ne_10m_lakes"),
        (layers.rivers, "ne_rivers", "ne_10m_rivers_lake_centerlines"),
        (layers.coastline, "ne_coastline", "ne_10m_coastline"),
    ] {
        run_command(
            state,
            "ogr2ogr",
            physical_vector_geopackage_arguments(source, geopackage, layer_name, source_layer_name),
            working_dir,
        )
        .await?;
    }
    Ok(())
}

fn country_label_geopackage_arguments(
    source: &Path,
    destination: &Path,
) -> Vec<std::ffi::OsString> {
    vec![
        "-f".into(),
        "GPKG".into(),
        "-nln".into(),
        "ne_places".into(),
        "-nlt".into(),
        "POINT".into(),
        "-dialect".into(),
        "SQLite".into(),
        "-sql".into(),
        "SELECT 'country' AS class, NAME AS name, NAME AS name_en, ST_PointOnSurface(geometry) AS geometry FROM ne_10m_admin_0_countries".into(),
        destination.as_os_str().to_owned(),
        source.as_os_str().to_owned(),
    ]
}

fn populated_place_label_geopackage_arguments(
    source: &Path,
    destination: &Path,
) -> Vec<std::ffi::OsString> {
    vec![
        "-update".into(),
        "-append".into(),
        "-nln".into(),
        "ne_places".into(),
        "-nlt".into(),
        "POINT".into(),
        "-dialect".into(),
        "SQLite".into(),
        "-sql".into(),
        "SELECT CASE WHEN SCALERANK <= 3 THEN 'city' WHEN SCALERANK <= 6 THEN 'town' ELSE 'village' END AS class, NAME AS name, COALESCE(NAMEASCII, NAME) AS name_en, geometry FROM ne_10m_populated_places".into(),
        destination.as_os_str().to_owned(),
        source.as_os_str().to_owned(),
    ]
}

fn boundary_geopackage_arguments(source: &Path, destination: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "-update".into(),
        "-append".into(),
        "-nln".into(),
        "ne_boundaries".into(),
        "-nlt".into(),
        "PROMOTE_TO_MULTI".into(),
        "-dialect".into(),
        "SQLite".into(),
        "-sql".into(),
        "SELECT geometry FROM ne_10m_admin_0_boundary_lines_land".into(),
        destination.as_os_str().to_owned(),
        source.as_os_str().to_owned(),
    ]
}

fn physical_vector_geopackage_arguments(
    source: &Path,
    destination: &Path,
    layer_name: &str,
    source_layer_name: &str,
) -> Vec<std::ffi::OsString> {
    vec![
        "-update".into(),
        "-append".into(),
        "-nln".into(),
        layer_name.into(),
        "-nlt".into(),
        "PROMOTE_TO_MULTI".into(),
        "-dialect".into(),
        "SQLite".into(),
        "-sql".into(),
        format!("SELECT geometry FROM {source_layer_name}").into(),
        destination.as_os_str().to_owned(),
        source.as_os_str().to_owned(),
    ]
}

fn vector_mbtiles_arguments(
    source: &Path,
    destination: &Path,
    name: &str,
    description: &str,
    mbtiles_type: &str,
    max_zoom: &str,
) -> Vec<std::ffi::OsString> {
    vec![
        "-f".into(),
        "MBTILES".into(),
        "-dsco".into(),
        format!("NAME={name}").into(),
        "-dsco".into(),
        format!("DESCRIPTION={description}").into(),
        "-dsco".into(),
        format!("TYPE={mbtiles_type}").into(),
        "-dsco".into(),
        "MINZOOM=0".into(),
        "-dsco".into(),
        format!("MAXZOOM={max_zoom}").into(),
        destination.as_os_str().to_owned(),
        source.as_os_str().to_owned(),
    ]
}

async fn run_command(
    state: &ServerState,
    program: &str,
    args: impl IntoIterator<Item = std::ffi::OsString>,
    working_dir: &Path,
) -> Result<()> {
    let args = args.into_iter().collect::<Vec<_>>();
    let command_display = format_command(program, &args);
    append_import_log(state, format!("Running command: {command_display}")).await;
    let output = match timeout(
        Duration::from_secs(NATURAL_EARTH_COMMAND_TIMEOUT_SECS),
        Command::new(program)
            .args(&args)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let error = format!("`{command_display}` could not be started: {error}");
            append_import_log(state, format!("Command failed: {error}")).await;
            bail!("map conversion command {error}")
        }
        Err(_) => {
            let error = format!(
                "`{command_display}` timed out after {NATURAL_EARTH_COMMAND_TIMEOUT_SECS} seconds"
            );
            append_import_log(state, format!("Command failed: {error}")).await;
            bail!("map conversion command {error}")
        }
    };
    if output.status.success() {
        let output_details =
            command_output_details(&output, NATURAL_EARTH_COMMAND_LOG_OUTPUT_LIMIT);
        let message = if output_details.is_empty() {
            format!("Command completed successfully: {command_display}")
        } else {
            format!("Command completed successfully: {command_display}\n{output_details}")
        };
        append_import_log(state, message).await;
        return Ok(());
    }
    let error = command_failure_message(&command_display, &output);
    append_import_log(state, format!("Command failed: {error}")).await;
    bail!("map conversion command {error}")
}

fn format_command(program: &str, args: &[std::ffi::OsString]) -> String {
    std::iter::once(program.to_string())
        .chain(
            args.iter()
                .map(|arg| quote_command_argument(&arg.to_string_lossy())),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_command_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/=:".contains(character))
    {
        argument.to_string()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

fn command_failure_message(command_display: &str, output: &std::process::Output) -> String {
    let details = command_output_details(output, NATURAL_EARTH_COMMAND_OUTPUT_LIMIT);
    if details.is_empty() {
        format!("`{command_display}` exited with {}", output.status)
    } else {
        format!(
            "`{command_display}` exited with {}:\n{details}",
            output.status
        )
    }
}

fn command_output_details(output: &std::process::Output, limit: usize) -> String {
    let mut sections = Vec::new();
    if let Some(stderr) = truncate_command_output(&output.stderr, limit) {
        sections.push(format!("stderr:\n{stderr}"));
    }
    if let Some(stdout) = truncate_command_output(&output.stdout, limit) {
        sections.push(format!("stdout:\n{stdout}"));
    }
    sections.join("\n")
}

fn truncate_command_output(output: &[u8], limit: usize) -> Option<String> {
    let clipped = &output[..output.len().min(limit)];
    let text = String::from_utf8_lossy(clipped).trim().to_string();
    if text.is_empty() {
        None
    } else {
        let truncated = output.len() > clipped.len();
        Some(if truncated {
            format!("{text}\n… output truncated")
        } else {
            text
        })
    }
}

fn find_layer(root: &Path, target_file_name: &str) -> Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).with_context(|| {
            format!(
                "failed reading extracted Natural Earth directory {}",
                directory.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(target_file_name)
            {
                return Ok(path);
            }
        }
    }
    bail!("Natural Earth archive does not contain required layer {target_file_name}")
}

async fn validate_generated_mbtiles(state: &ServerState, path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("failed opening generated MBTiles artifact")?;
        let tiles: i64 = connection
            .query_row("select count(*) from tiles", [], |row| row.get(0))
            .context("generated MBTiles artifact has no readable tiles table")?;
        if tiles == 0 {
            bail!("generated MBTiles artifact contains no tiles")
        }
        let format: Option<String> = connection
            .query_row(
                "select value from metadata where name = 'format' limit 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("generated MBTiles artifact has no readable metadata table")?;
        if !matches!(format.as_deref(), Some(value) if value.eq_ignore_ascii_case("png")) {
            bail!("generated MBTiles artifact must declare PNG tiles")
        }
        Ok(())
    })
    .await
    .context("generated MBTiles validation task failed")??;
    append_import_log(state, "Generated MBTiles validation completed successfully").await;
    Ok(())
}

async fn validate_generated_label_mbtiles(state: &ServerState, path: &Path) -> Result<()> {
    validate_generated_vector_mbtiles(
        state,
        path,
        "Natural Earth label overlay",
        &["ne_places", "ne_boundaries"],
    )
    .await
}

async fn validate_generated_vector_mbtiles(
    state: &ServerState,
    path: &Path,
    artifact_description: &str,
    required_layers: &[&str],
) -> Result<()> {
    let path = path.to_path_buf();
    let artifact_description = artifact_description.to_string();
    let validation_description = artifact_description.clone();
    let required_layers = required_layers
        .iter()
        .map(|layer| (*layer).to_string())
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| {
                format!("failed opening generated {validation_description} MBTiles artifact")
            })?;
        let tiles: i64 = connection
            .query_row("select count(*) from tiles", [], |row| row.get(0))
            .with_context(|| {
                format!("generated {validation_description} artifact has no readable tiles table")
            })?;
        if tiles == 0 {
            bail!("generated {validation_description} artifact contains no tiles")
        }
        let format: Option<String> = connection
            .query_row(
                "select value from metadata where name = 'format' limit 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| {
                format!(
                    "generated {validation_description} artifact has no readable metadata table"
                )
            })?;
        if !matches!(format.as_deref(), Some(value) if value.eq_ignore_ascii_case("pbf")) {
            bail!("generated {validation_description} artifact must declare PBF vector tiles")
        }
        let metadata_json: Option<String> = connection
            .query_row(
                "select value from metadata where name = 'json' limit 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| {
                format!("generated {validation_description} artifact has no vector metadata")
            })?;
        validate_natural_earth_vector_metadata(
            metadata_json.as_deref(),
            &required_layers,
            &validation_description,
        )
    })
    .await
    .with_context(|| format!("generated {artifact_description} validation task failed"))??;
    append_import_log(
        state,
        format!("Generated {artifact_description} validation completed successfully"),
    )
    .await;
    Ok(())
}

#[cfg(test)]
fn validate_natural_earth_label_metadata(metadata_json: Option<&str>) -> Result<()> {
    validate_natural_earth_vector_metadata(
        metadata_json,
        &["ne_places".to_string(), "ne_boundaries".to_string()],
        "Natural Earth label",
    )
}

fn validate_natural_earth_vector_metadata(
    metadata_json: Option<&str>,
    required_layers: &[String],
    artifact_description: &str,
) -> Result<()> {
    let metadata_json = metadata_json.with_context(|| {
        format!("generated {artifact_description} artifact has no json metadata")
    })?;
    let metadata: serde_json::Value = serde_json::from_str(metadata_json)
        .with_context(|| format!("generated {artifact_description} metadata is invalid JSON"))?;
    let vector_layers = metadata
        .get("vector_layers")
        .and_then(serde_json::Value::as_array)
        .with_context(|| {
            format!("generated {artifact_description} metadata has no vector_layers array")
        })?;
    let layer_names = vector_layers
        .iter()
        .filter_map(|layer| layer.get("id").and_then(serde_json::Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    let missing_layers = required_layers
        .iter()
        .filter(|layer| !layer_names.contains(layer.as_str()))
        .collect::<Vec<_>>();
    if !missing_layers.is_empty() {
        bail!(
            "generated {artifact_description} artifact is missing required vector layer(s): {}",
            missing_layers
                .iter()
                .map(|layer| layer.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
    Ok(())
}

fn required_job_artifact(
    job: &NaturalEarthImportJobView,
    asset: map_config::MapVariantAssetKind,
) -> Result<&NaturalEarthImportArtifactView> {
    job.artifacts
        .iter()
        .find(|artifact| artifact.asset == asset)
        .with_context(|| format!("Natural Earth import job has no configured {asset:?} artifact"))
}

async fn record_published_artifact(
    state: &ServerState,
    manifest_key: &str,
    logical_size_bytes: u64,
) {
    let mut runtime = state.storage.natural_earth_import.lock().await;
    if let Some(job) = runtime.job.as_mut()
        && matches!(job.state, NaturalEarthImportState::Running)
    {
        if let Some(artifact) = job
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.manifest_key == manifest_key)
        {
            artifact.logical_size_bytes = logical_size_bytes;
        }
        job.logical_size_bytes = job.artifacts.iter().fold(0_u64, |total, artifact| {
            total.saturating_add(artifact.logical_size_bytes)
        });
        job.updated_at_unix = unix_ts();
    }
}

async fn publish_generated_mbtiles(
    state: &ServerState,
    job: &NaturalEarthImportJobView,
    artifact: &NaturalEarthImportArtifactView,
    artifact_path: &Path,
) -> Result<u64> {
    publish_generated_mbtiles_with_limits(
        state,
        job,
        artifact,
        artifact_path,
        NATURAL_EARTH_IMPORT_MAX_ARTIFACT_BYTES,
        NATURAL_EARTH_IMPORT_PART_BYTES,
    )
    .await
}

async fn publish_generated_mbtiles_with_limits(
    state: &ServerState,
    job: &NaturalEarthImportJobView,
    artifact: &NaturalEarthImportArtifactView,
    artifact_path: &Path,
    max_artifact_bytes: usize,
    part_bytes: usize,
) -> Result<u64> {
    let payload = fs::read(artifact_path).await.with_context(|| {
        format!(
            "failed reading generated MBTiles {}",
            artifact_path.display()
        )
    })?;
    if payload.is_empty() {
        bail!("generated MBTiles artifact is empty")
    }
    if payload.len() > max_artifact_bytes {
        bail!("generated MBTiles artifact exceeds the configured artifact size limit")
    }
    if part_bytes == 0 {
        bail!("generated MBTiles artifact part size must be greater than zero")
    }
    let mut parts = Vec::new();
    let mut offset = 0_u64;
    for (index, part_payload) in payload.chunks(part_bytes).enumerate() {
        let size_bytes =
            u64::try_from(part_payload.len()).context("map artifact part size overflow")?;
        let key = format!(
            "{}.natural-earth-{}-part-{}",
            artifact.logical_key,
            job.id,
            split_part_id(index)?
        );
        put_system_object(state, &key, Bytes::copy_from_slice(part_payload)).await?;
        parts.push(SplitFileManifestPartDocument {
            part_id: split_part_id(index)?,
            key,
            offset_bytes: offset,
            size_bytes,
        });
        offset = offset
            .checked_add(size_bytes)
            .context("map artifact size overflow")?;
    }
    let parts_count = parts.len();
    let manifest = SplitFileManifestDocument {
        manifest_version: 1,
        manifest_type: "split_file_manifest",
        logical_format: "mbtiles",
        logical_key: artifact.logical_key.clone(),
        manifest_key: artifact.manifest_key.clone(),
        storage_root: "sys/maps",
        logical_size_bytes: offset,
        last_part_size_bytes: parts.last().map(|part| part.size_bytes).unwrap_or_default(),
        parts_count,
        parts,
    };
    put_system_object(
        state,
        &artifact.manifest_key,
        Bytes::from(
            serde_json::to_vec_pretty(&manifest)
                .context("failed serializing map artifact manifest")?,
        ),
    )
    .await?;
    map_dataset_import::invalidate_cached_mbtiles_source(state, &artifact.manifest_key).await;
    append_import_log(
        state,
        format!("Published {parts_count} map artifact part(s) and manifest"),
    )
    .await;
    Ok(offset)
}

async fn put_system_object(state: &ServerState, key: &str, payload: Bytes) -> Result<()> {
    let outcome = {
        let mut store = lock_store(state, "natural_earth_import.put").await;
        store
            .put_object_versioned(key, payload, PutOptions::default())
            .await
            .with_context(|| format!("failed storing Natural Earth map object {key}"))?
    };
    map_dataset_import::register_put_outcome(state, key, &outcome.version_id).await;
    Ok(())
}

fn split_part_id(index: usize) -> Result<String> {
    let first = u8::try_from(index / 26).context("too many Natural Earth map parts")?;
    let second = u8::try_from(index % 26).context("Natural Earth map part index overflow")?;
    if first >= 26 {
        bail!("too many Natural Earth map parts")
    }
    Ok(format!(
        "{}{}",
        char::from(b'a' + first),
        char::from(b'a' + second)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_artifact(
        variant_id: &str,
        asset: map_config::MapVariantAssetKind,
    ) -> NaturalEarthImportArtifactView {
        let logical_key = format!("sys/maps/{variant_id}.mbtiles");
        NaturalEarthImportArtifactView {
            variant_id: variant_id.to_string(),
            asset,
            manifest_key: format!("{logical_key}.manifest.json"),
            logical_key,
            logical_size_bytes: 0,
        }
    }

    fn test_job(
        profile: NaturalEarthImportProfile,
        artifacts: Vec<NaturalEarthImportArtifactView>,
    ) -> NaturalEarthImportJobView {
        let primary = artifacts.first().expect("a test job needs an artifact");
        NaturalEarthImportJobView {
            id: "natural-earth-test-job".to_string(),
            state: NaturalEarthImportState::Running,
            profile,
            phase: "Queued".to_string(),
            source_url: natural_earth_source_url(profile).to_string(),
            logical_key: primary.logical_key.clone(),
            manifest_key: primary.manifest_key.clone(),
            logical_size_bytes: 0,
            artifacts,
            error: None,
            log_entries: Vec::new(),
            started_at_unix: 1,
            updated_at_unix: 1,
        }
    }

    fn write_mbtiles_fixture(
        path: &Path,
        include_tile: bool,
        format: Option<&str>,
        metadata_json: Option<&str>,
    ) {
        let connection = Connection::open(path).expect("the MBTiles fixture should open");
        connection
            .execute_batch(
                "create table tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob);\
                 create table metadata (name text, value text);",
            )
            .expect("the MBTiles fixture schema should be created");
        if include_tile {
            connection
                .execute(
                    "insert into tiles (zoom_level, tile_column, tile_row, tile_data) values (?1, ?2, ?3, ?4)",
                    (0_i64, 0_i64, 0_i64, vec![1_u8, 2, 3]),
                )
                .expect("the MBTiles fixture tile should be inserted");
        }
        if let Some(format) = format {
            connection
                .execute(
                    "insert into metadata (name, value) values (?1, ?2)",
                    ("format", format),
                )
                .expect("the MBTiles fixture format should be inserted");
        }
        if let Some(metadata_json) = metadata_json {
            connection
                .execute(
                    "insert into metadata (name, value) values (?1, ?2)",
                    ("json", metadata_json),
                )
                .expect("the MBTiles fixture metadata should be inserted");
        }
    }

    fn assert_error_contains(error: anyhow::Error, expected: &str) {
        let message = format!("{error:#}");
        assert!(
            message.contains(expected),
            "expected {message:?} to contain {expected:?}"
        );
    }

    async fn wait_for_terminal_job(state: &ServerState) -> NaturalEarthImportJobView {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let job = state
                    .storage
                    .natural_earth_import
                    .lock()
                    .await
                    .job
                    .clone()
                    .expect("the Natural Earth job should exist");
                if !matches!(job.state, NaturalEarthImportState::Running) {
                    return job;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the Natural Earth test worker should finish within ten seconds")
    }

    #[test]
    fn split_part_ids_are_stable() {
        assert_eq!(split_part_id(0).unwrap(), "aa");
        assert_eq!(split_part_id(25).unwrap(), "az");
        assert_eq!(split_part_id(26).unwrap(), "ba");
    }

    #[test]
    fn command_probes_use_unzip_version_flag() {
        assert_eq!(
            REQUIRED_PHYSICAL_IMPORT_COMMANDS
                .iter()
                .find(|(command, _)| *command == "unzip"),
            Some(&("unzip", "-v"))
        );
    }

    #[test]
    fn command_display_names_the_exact_command() {
        assert_eq!(format_command("unzip", &["-v".into()]), "unzip -v");
    }

    #[test]
    fn vector_profiles_require_ogr2ogr_but_the_raster_only_profile_does_not() {
        assert!(
            !required_import_commands(NaturalEarthImportProfile::Physical)
                .iter()
                .any(|(command, _)| *command == "ogr2ogr")
        );
        assert!(
            required_import_commands(NaturalEarthImportProfile::PhysicalWithLabels)
                .iter()
                .any(|(command, argument)| *command == "ogr2ogr" && *argument == "--version")
        );
        assert_eq!(
            natural_earth_source_url(NaturalEarthImportProfile::PhysicalVector),
            NATURAL_EARTH_PHYSICAL_10M_URL
        );
        let vector_commands = required_import_commands(NaturalEarthImportProfile::PhysicalVector);
        assert!(
            vector_commands
                .iter()
                .any(|(command, argument)| *command == "ogr2ogr" && *argument == "--version")
        );
        assert!(
            !vector_commands
                .iter()
                .any(|(command, _)| *command == "gdal_rasterize")
        );
    }

    #[test]
    fn cross_blended_hypso_profile_uses_the_official_raster_source_without_vector_tools() {
        assert_eq!(
            natural_earth_source_url(NaturalEarthImportProfile::CrossBlendedHypso),
            NATURAL_EARTH_CROSS_BLENDED_HYPSO_10M_URL
        );
        let commands = required_import_commands(NaturalEarthImportProfile::CrossBlendedHypso);
        assert!(commands.iter().any(|(command, _)| *command == "gdalwarp"));
        assert!(
            !commands
                .iter()
                .any(|(command, _)| *command == "gdal_rasterize")
        );
        assert!(!commands.iter().any(|(command, _)| *command == "ogr2ogr"));
    }

    #[test]
    fn natural_earth_one_profile_uses_the_official_relief_and_water_raster() {
        assert_eq!(
            natural_earth_source_url(NaturalEarthImportProfile::NaturalEarthOne),
            NATURAL_EARTH_ONE_SHADED_RELIEF_WATER_10M_URL
        );
        let commands = required_import_commands(NaturalEarthImportProfile::NaturalEarthOne);
        assert!(commands.iter().any(|(command, _)| *command == "gdalwarp"));
        assert!(
            !commands
                .iter()
                .any(|(command, _)| *command == "gdal_rasterize")
        );
        assert!(!commands.iter().any(|(command, _)| *command == "ogr2ogr"));
    }

    #[test]
    fn label_conversion_uses_the_viewer_source_layer_contract() {
        let countries = Path::new("/tmp/ne_10m_admin_0_countries.shp");
        let places = Path::new("/tmp/ne_10m_populated_places.shp");
        let boundaries = Path::new("/tmp/ne_10m_admin_0_boundary_lines_land.shp");
        let geopackage = Path::new("/tmp/natural-earth-labels.gpkg");
        let artifact = Path::new("/tmp/natural-earth-labels.mbtiles");

        let country_arguments = country_label_geopackage_arguments(countries, geopackage);
        let place_arguments = populated_place_label_geopackage_arguments(places, geopackage);
        let boundary_arguments = boundary_geopackage_arguments(boundaries, geopackage);
        let mbtiles_command = format_command(
            "ogr2ogr",
            &vector_mbtiles_arguments(
                geopackage,
                artifact,
                "Natural Earth labels",
                "Natural Earth countries, populated places, and borders",
                "overlay",
                NATURAL_EARTH_LABEL_MAX_ZOOM,
            ),
        );

        let country_sql = country_arguments[9].to_string_lossy();
        let place_sql = place_arguments[9].to_string_lossy();
        assert!(country_sql.contains("'country' AS class"));
        assert!(country_sql.contains("ST_PointOnSurface(geometry)"));
        assert!(place_sql.contains("'city'"));
        assert!(place_sql.contains("'town'"));
        assert!(place_sql.contains("'village'"));
        assert_eq!(boundary_arguments[3].to_string_lossy(), "ne_boundaries");
        assert!(mbtiles_command.contains("-f MBTILES"));
        assert!(mbtiles_command.contains("MINZOOM=0"));
        assert!(mbtiles_command.contains("MAXZOOM=8"));
    }

    #[test]
    fn physical_vector_conversion_uses_all_viewer_source_layers() {
        let geopackage = Path::new("/tmp/natural-earth-vector.gpkg");
        let ocean = Path::new("/tmp/ne_10m_ocean.shp");
        let vector_arguments =
            physical_vector_geopackage_arguments(ocean, geopackage, "ne_ocean", "ne_10m_ocean");
        let mbtiles_command = format_command(
            "ogr2ogr",
            &vector_mbtiles_arguments(
                geopackage,
                Path::new("/tmp/natural-earth-vector.mbtiles"),
                "Natural Earth Vector",
                "Natural Earth physical world map",
                "baselayer",
                NATURAL_EARTH_VECTOR_MAX_ZOOM,
            ),
        );

        assert_eq!(vector_arguments[3].to_string_lossy(), "ne_ocean");
        assert!(
            vector_arguments
                .iter()
                .any(|argument| argument == "SELECT geometry FROM ne_10m_ocean")
        );
        assert!(mbtiles_command.contains("TYPE=baselayer"));
        assert!(mbtiles_command.contains("MAXZOOM=6"));
        assert!(
            validate_natural_earth_vector_metadata(
                Some(
                    r#"{"vector_layers":[{"id":"ne_land"},{"id":"ne_ocean"},{"id":"ne_lakes"},{"id":"ne_rivers"},{"id":"ne_coastline"},{"id":"ne_places"},{"id":"ne_boundaries"}]}"#
                ),
                &[
                    "ne_land".to_string(),
                    "ne_ocean".to_string(),
                    "ne_lakes".to_string(),
                    "ne_rivers".to_string(),
                    "ne_coastline".to_string(),
                    "ne_places".to_string(),
                    "ne_boundaries".to_string(),
                ],
                "Natural Earth vector map"
            )
            .is_ok()
        );
    }

    #[test]
    fn label_metadata_requires_places_and_boundaries_layers() {
        assert!(
            validate_natural_earth_label_metadata(Some(
                r#"{"vector_layers":[{"id":"ne_places"},{"id":"ne_boundaries"}]}"#
            ))
            .is_ok()
        );
        assert!(
            validate_natural_earth_label_metadata(Some(
                r#"{"vector_layers":[{"id":"ne_places"}]}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn raster_conversion_arguments_preserve_world_extent_and_rgba_band_order() {
        let source = Path::new("/tmp/ne_10m_ocean.shp");
        let destination = Path::new("/tmp/natural-earth.tif");
        let create = rasterize_create_arguments(source, destination, [10, 20, 30, 40])
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            create,
            [
                "-of",
                "GTiff",
                "-te",
                "-180",
                "-90",
                "180",
                "90",
                "-ts",
                "4096",
                "2048",
                "-a_srs",
                "EPSG:4326",
                "-ot",
                "Byte",
                "-burn",
                "10",
                "-burn",
                "20",
                "-burn",
                "30",
                "-burn",
                "40",
                "/tmp/ne_10m_ocean.shp",
                "/tmp/natural-earth.tif",
            ]
        );

        let overlay = rasterize_overlay_arguments(source, destination, [10, 20, 30, 40])
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            overlay,
            [
                "-b",
                "1",
                "-burn",
                "10",
                "-b",
                "2",
                "-burn",
                "20",
                "-b",
                "3",
                "-burn",
                "30",
                "-b",
                "4",
                "-burn",
                "40",
                "/tmp/ne_10m_ocean.shp",
                "/tmp/natural-earth.tif",
            ]
        );
    }

    #[tokio::test]
    async fn import_worker_lifecycle_covers_every_profile_and_serializes_jobs() {
        let state = crate::tests::build_natural_earth_test_state().await;
        let profile_expectations = [
            (
                NaturalEarthImportProfile::Physical,
                vec![(
                    "natural-earth-globe",
                    map_config::MapVariantAssetKind::Raster,
                )],
            ),
            (
                NaturalEarthImportProfile::PhysicalWithLabels,
                vec![
                    (
                        "natural-earth-labels",
                        map_config::MapVariantAssetKind::Raster,
                    ),
                    (
                        "natural-earth-labels",
                        map_config::MapVariantAssetKind::Vector,
                    ),
                ],
            ),
            (
                NaturalEarthImportProfile::PhysicalVector,
                vec![(
                    "natural-earth-vector",
                    map_config::MapVariantAssetKind::Vector,
                )],
            ),
            (
                NaturalEarthImportProfile::CrossBlendedHypso,
                vec![(
                    "natural-earth-hypso",
                    map_config::MapVariantAssetKind::Raster,
                )],
            ),
            (
                NaturalEarthImportProfile::NaturalEarthOne,
                vec![("natural-earth-1", map_config::MapVariantAssetKind::Raster)],
            ),
        ];

        for (index, (profile, expected_artifacts)) in profile_expectations.into_iter().enumerate() {
            let expected_size = 100 + index as u64;
            let accepted =
                start_import_with_worker(
                    &state,
                    profile,
                    move |_worker_state, _worker_job| async move {
                        Ok::<u64, anyhow::Error>(expected_size)
                    },
                )
                .await
                .expect("the synthetic Natural Earth worker should start");
            assert_eq!(accepted.profile, profile);
            assert_eq!(accepted.artifacts.len(), expected_artifacts.len());
            for (artifact, (variant_id, asset)) in accepted.artifacts.iter().zip(expected_artifacts)
            {
                assert_eq!(artifact.variant_id, variant_id);
                assert_eq!(artifact.asset, asset);
            }

            let finished = wait_for_terminal_job(&state).await;
            assert!(matches!(finished.state, NaturalEarthImportState::Ready));
            assert_eq!(finished.phase, "Ready");
            assert_eq!(finished.logical_size_bytes, expected_size);
            assert_eq!(finished.error, None);
            assert!(
                finished
                    .log_entries
                    .iter()
                    .any(|entry| entry.message == "Import completed successfully")
            );
        }

        start_import_with_worker(&state, NaturalEarthImportProfile::Physical, |_, _| async {
            Err(anyhow!("synthetic conversion failure"))
        })
        .await
        .expect("the failing synthetic worker should still be accepted");
        let failed = wait_for_terminal_job(&state).await;
        assert!(matches!(failed.state, NaturalEarthImportState::Failed));
        assert_eq!(failed.phase, "Failed");
        assert_eq!(failed.logical_size_bytes, 0);
        assert_eq!(
            failed.error.as_deref(),
            Some("synthetic conversion failure")
        );

        let release_worker = Arc::new(tokio::sync::Notify::new());
        let worker_release = release_worker.clone();
        start_import_with_worker(&state, NaturalEarthImportProfile::Physical, move |_, _| {
            let worker_release = worker_release.clone();
            async move {
                worker_release.notified().await;
                Ok(7)
            }
        })
        .await
        .expect("the blocked synthetic worker should start");
        assert!(is_running(&state).await);
        let conflict = start_import_with_worker(
            &state,
            NaturalEarthImportProfile::PhysicalVector,
            |_, _| async { Ok(1) },
        )
        .await
        .expect_err("a second Natural Earth worker must be rejected");
        assert_eq!(conflict.0, StatusCode::CONFLICT);
        assert!(
            conflict
                .1
                .to_string()
                .contains("conversion is already running")
        );
        release_worker.notify_one();
        assert!(matches!(
            wait_for_terminal_job(&state).await.state,
            NaturalEarthImportState::Ready
        ));

        crate::tests::cleanup_natural_earth_test_state(&state).await;
    }

    #[tokio::test]
    async fn generated_raster_artifact_is_validated_published_and_accounted() {
        let state = crate::tests::build_natural_earth_test_state().await;
        let artifact = test_artifact(
            "natural-earth-globe",
            map_config::MapVariantAssetKind::Raster,
        );
        let job = test_job(NaturalEarthImportProfile::Physical, vec![artifact.clone()]);
        state.storage.natural_earth_import.lock().await.job = Some(job.clone());
        let artifact_path = state
            .managed_paths
            .data_dir()
            .join("generated-natural-earth.mbtiles");
        write_mbtiles_fixture(&artifact_path, true, Some("PNG"), None);

        assert!(is_running(&state).await);
        update_phase(&state, "Validating the generated raster").await;
        validate_generated_mbtiles(&state, &artifact_path)
            .await
            .expect("valid raster MBTiles should pass validation");
        assert_eq!(
            required_job_artifact(&job, map_config::MapVariantAssetKind::Raster)
                .expect("the raster artifact should be configured")
                .manifest_key,
            artifact.manifest_key
        );
        assert_error_contains(
            required_job_artifact(&job, map_config::MapVariantAssetKind::Vector)
                .expect_err("the raster-only job must not expose a vector artifact"),
            "no configured Vector artifact",
        );

        let source_payload = fs::read(&artifact_path)
            .await
            .expect("the generated artifact should be readable");
        let published_size = publish_generated_mbtiles(&state, &job, &artifact, &artifact_path)
            .await
            .expect("the generated artifact should publish");
        assert_eq!(published_size, source_payload.len() as u64);
        record_published_artifact(&state, &artifact.manifest_key, published_size).await;

        let manifest_payload = {
            let store = read_store(&state, "test.natural_earth.read_manifest").await;
            store
                .get_object(
                    &artifact.manifest_key,
                    None,
                    None,
                    storage::ObjectReadMode::Preferred,
                )
                .await
                .expect("the published manifest should be readable")
        };
        let manifest: serde_json::Value =
            serde_json::from_slice(&manifest_payload).expect("the manifest should be JSON");
        assert_eq!(manifest["type"], "split_file_manifest");
        assert_eq!(manifest["logical_key"], artifact.logical_key);
        assert_eq!(manifest["logical_size_bytes"], published_size);
        assert_eq!(manifest["last_part_size_bytes"], published_size);
        assert_eq!(manifest["parts_count"], 1);
        assert_eq!(manifest["parts"][0]["part_id"], "aa");
        assert_eq!(manifest["parts"][0]["offset_bytes"], 0);
        assert_eq!(manifest["parts"][0]["size_bytes"], published_size);
        let part_key = manifest["parts"][0]["key"]
            .as_str()
            .expect("the manifest part should have a key");
        assert_eq!(
            part_key,
            "sys/maps/natural-earth-globe.mbtiles.natural-earth-natural-earth-test-job-part-aa"
        );
        let published_payload = {
            let store = read_store(&state, "test.natural_earth.read_part").await;
            store
                .get_object(part_key, None, None, storage::ObjectReadMode::Preferred)
                .await
                .expect("the published part should be readable")
        };
        assert_eq!(published_payload.as_ref(), source_payload.as_slice());

        finish_job(
            &state,
            NaturalEarthImportState::Ready,
            "Ready",
            published_size,
            None,
        )
        .await;
        {
            let runtime = state.storage.natural_earth_import.lock().await;
            let finished = runtime.job.as_ref().expect("the job should remain visible");
            assert!(matches!(finished.state, NaturalEarthImportState::Ready));
            assert_eq!(finished.logical_size_bytes, published_size);
            assert_eq!(finished.artifacts[0].logical_size_bytes, published_size);
            assert!(
                finished
                    .log_entries
                    .iter()
                    .any(|entry| entry.message.contains("Published 1 map artifact part"))
            );
        }
        assert!(!is_running(&state).await);

        let empty_artifact = state.managed_paths.data_dir().join("empty.mbtiles");
        fs::write(&empty_artifact, [])
            .await
            .expect("the empty artifact should be created");
        assert_error_contains(
            publish_generated_mbtiles(&state, &job, &artifact, &empty_artifact)
                .await
                .expect_err("an empty generated artifact must be rejected"),
            "artifact is empty",
        );

        let multipart_artifact = test_artifact(
            "natural-earth-multipart",
            map_config::MapVariantAssetKind::Raster,
        );
        let multipart_job = test_job(
            NaturalEarthImportProfile::Physical,
            vec![multipart_artifact.clone()],
        );
        let multipart_path = state.managed_paths.data_dir().join("multipart.mbtiles");
        let multipart_payload = b"abcdefghij";
        fs::write(&multipart_path, multipart_payload)
            .await
            .expect("the multipart artifact should be created");
        let multipart_size = publish_generated_mbtiles_with_limits(
            &state,
            &multipart_job,
            &multipart_artifact,
            &multipart_path,
            32,
            4,
        )
        .await
        .expect("the multipart artifact should publish");
        assert_eq!(multipart_size, multipart_payload.len() as u64);

        let multipart_manifest_payload = {
            let store = read_store(&state, "test.natural_earth.read_multipart_manifest").await;
            store
                .get_object(
                    &multipart_artifact.manifest_key,
                    None,
                    None,
                    storage::ObjectReadMode::Preferred,
                )
                .await
                .expect("the multipart manifest should be readable")
        };
        let multipart_manifest: serde_json::Value =
            serde_json::from_slice(&multipart_manifest_payload)
                .expect("the multipart manifest should be JSON");
        assert_eq!(multipart_manifest["logical_size_bytes"], 10);
        assert_eq!(multipart_manifest["last_part_size_bytes"], 2);
        assert_eq!(multipart_manifest["parts_count"], 3);
        let multipart_parts = multipart_manifest["parts"]
            .as_array()
            .expect("the multipart manifest should list its parts");
        for (index, (part, expected_payload)) in multipart_parts
            .iter()
            .zip([b"abcd".as_slice(), b"efgh".as_slice(), b"ij".as_slice()])
            .enumerate()
        {
            assert_eq!(part["part_id"], ["aa", "ab", "ac"][index]);
            assert_eq!(part["offset_bytes"], (index * 4) as u64);
            assert_eq!(part["size_bytes"], expected_payload.len() as u64);
            let part_key = part["key"]
                .as_str()
                .expect("each multipart manifest entry should have a key");
            let stored_payload = {
                let store = read_store(&state, "test.natural_earth.read_multipart_part").await;
                store
                    .get_object(part_key, None, None, storage::ObjectReadMode::Preferred)
                    .await
                    .expect("each multipart payload should be readable")
            };
            assert_eq!(stored_payload.as_ref(), expected_payload);
        }

        let oversized_path = state.managed_paths.data_dir().join("oversized.mbtiles");
        fs::write(&oversized_path, b"12345")
            .await
            .expect("the oversized artifact should be created");
        assert_error_contains(
            publish_generated_mbtiles_with_limits(
                &state,
                &multipart_job,
                &multipart_artifact,
                &oversized_path,
                4,
                2,
            )
            .await
            .expect_err("an oversized artifact must be rejected"),
            "exceeds the configured artifact size limit",
        );
        assert_error_contains(
            publish_generated_mbtiles_with_limits(
                &state,
                &multipart_job,
                &multipart_artifact,
                &multipart_path,
                32,
                0,
            )
            .await
            .expect_err("a zero artifact part size must be rejected"),
            "part size must be greater than zero",
        );

        crate::tests::cleanup_natural_earth_test_state(&state).await;
    }

    #[tokio::test]
    async fn mbtiles_validation_reports_raster_and_vector_contract_failures() {
        let state = crate::tests::build_natural_earth_test_state().await;
        let fixture_root = state.managed_paths.data_dir().join("mbtiles-validation");
        fs::create_dir_all(&fixture_root)
            .await
            .expect("the validation fixture directory should be created");

        let blank = fixture_root.join("blank.mbtiles");
        drop(Connection::open(&blank).expect("the blank SQLite fixture should open"));
        assert_error_contains(
            validate_generated_mbtiles(&state, &blank)
                .await
                .expect_err("an MBTiles database without a tiles table must fail"),
            "no readable tiles table",
        );

        let empty_raster = fixture_root.join("empty-raster.mbtiles");
        write_mbtiles_fixture(&empty_raster, false, Some("png"), None);
        assert_error_contains(
            validate_generated_mbtiles(&state, &empty_raster)
                .await
                .expect_err("a raster MBTiles database without tiles must fail"),
            "contains no tiles",
        );

        let jpeg_raster = fixture_root.join("jpeg-raster.mbtiles");
        write_mbtiles_fixture(&jpeg_raster, true, Some("jpeg"), None);
        assert_error_contains(
            validate_generated_mbtiles(&state, &jpeg_raster)
                .await
                .expect_err("a raster MBTiles database with JPEG metadata must fail"),
            "must declare PNG tiles",
        );

        let empty_vector = fixture_root.join("empty-vector.mbtiles");
        write_mbtiles_fixture(&empty_vector, false, Some("pbf"), Some("{}"));
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &empty_vector)
                .await
                .expect_err("a vector MBTiles database without tiles must fail"),
            "contains no tiles",
        );

        let wrong_vector_format = fixture_root.join("wrong-vector-format.mbtiles");
        write_mbtiles_fixture(
            &wrong_vector_format,
            true,
            Some("png"),
            Some(r#"{"vector_layers":[]}"#),
        );
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &wrong_vector_format)
                .await
                .expect_err("a vector MBTiles database with PNG metadata must fail"),
            "must declare PBF vector tiles",
        );

        let missing_json = fixture_root.join("missing-json.mbtiles");
        write_mbtiles_fixture(&missing_json, true, Some("pbf"), None);
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &missing_json)
                .await
                .expect_err("vector metadata is required"),
            "has no json metadata",
        );

        let invalid_json = fixture_root.join("invalid-json.mbtiles");
        write_mbtiles_fixture(&invalid_json, true, Some("pbf"), Some("{"));
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &invalid_json)
                .await
                .expect_err("malformed vector metadata must fail"),
            "metadata is invalid JSON",
        );

        let missing_array = fixture_root.join("missing-array.mbtiles");
        write_mbtiles_fixture(
            &missing_array,
            true,
            Some("pbf"),
            Some(r#"{"name":"labels"}"#),
        );
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &missing_array)
                .await
                .expect_err("vector metadata without vector_layers must fail"),
            "has no vector_layers array",
        );

        let missing_layer = fixture_root.join("missing-layer.mbtiles");
        write_mbtiles_fixture(
            &missing_layer,
            true,
            Some("pbf"),
            Some(r#"{"vector_layers":[{"id":"ne_places"}]}"#),
        );
        assert_error_contains(
            validate_generated_label_mbtiles(&state, &missing_layer)
                .await
                .expect_err("all required label layers must be present"),
            "missing required vector layer(s): ne_boundaries",
        );

        let valid_vector = fixture_root.join("valid-vector.mbtiles");
        write_mbtiles_fixture(
            &valid_vector,
            true,
            Some("PBF"),
            Some(
                r#"{"vector_layers":[{"id":"ne_land"},{"id":"ne_ocean"},{"id":"ne_lakes"},{"id":"ne_rivers"},{"id":"ne_coastline"},{"id":"ne_places"},{"id":"ne_boundaries"}]}"#,
            ),
        );
        validate_generated_label_mbtiles(&state, &valid_vector)
            .await
            .expect("a complete label overlay should pass validation");
        validate_generated_vector_mbtiles(
            &state,
            &valid_vector,
            "Natural Earth vector map",
            &[
                "ne_land",
                "ne_ocean",
                "ne_lakes",
                "ne_rivers",
                "ne_coastline",
                "ne_places",
                "ne_boundaries",
            ],
        )
        .await
        .expect("a complete Natural Earth vector map should pass validation");

        crate::tests::cleanup_natural_earth_test_state(&state).await;
    }

    #[test]
    fn layer_lookup_is_recursive_case_insensitive_and_diagnostic() {
        let root = std::env::temp_dir().join(format!("natural-earth-layers-{}", Uuid::now_v7()));
        let nested = root.join("archive").join("Physical");
        std::fs::create_dir_all(&nested).expect("the layer fixture directory should be created");
        let expected = nested.join("NE_10M_LAND.SHP");
        std::fs::write(&expected, b"shape fixture").expect("the layer fixture should be written");

        assert_eq!(
            find_layer(&root, "ne_10m_land.shp").expect("the nested layer should be found"),
            expected
        );
        assert_error_contains(
            find_layer(&root, "ne_10m_ocean.shp").expect_err("a missing layer should be reported"),
            "does not contain required layer ne_10m_ocean.shp",
        );

        std::fs::remove_dir_all(root).expect("the layer fixture should be removed");
    }

    #[test]
    fn job_log_retains_only_the_most_recent_entries() {
        let artifact = test_artifact(
            "natural-earth-globe",
            map_config::MapVariantAssetKind::Raster,
        );
        let mut job = test_job(NaturalEarthImportProfile::Physical, vec![artifact]);
        for index in 0..=NATURAL_EARTH_IMPORT_LOG_MAX_ENTRIES {
            append_job_log(&mut job, format!("entry-{index}"));
        }

        assert_eq!(job.log_entries.len(), NATURAL_EARTH_IMPORT_LOG_MAX_ENTRIES);
        assert_eq!(job.log_entries[0].message, "entry-1");
        let expected_last = format!("entry-{NATURAL_EARTH_IMPORT_LOG_MAX_ENTRIES}");
        assert_eq!(
            job.log_entries.last().map(|entry| entry.message.as_str()),
            Some(expected_last.as_str())
        );
    }
}
