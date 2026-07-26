use super::*;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fs;

type HmacSha256 = Hmac<Sha256>;

const RELIABILITY_TELEMETRY_STATE_FILE: &str = "telemetry/reliability-telemetry-state.json";
const TELEMETRY_SCHEMA_VERSION: u32 = 1;
const TELEMETRY_HMAC_DOMAIN: &[u8] = b"ironmesh-telemetry-v1";

/// Central collector ingest URL. Per doc Section 5.2 the production collector is assumed to live
/// at `creax.de:44044`; the endpoint path matches `stats-collector-server`'s ingest route.
/// Overridable via `IRONMESH_RELIABILITY_TELEMETRY_COLLECTOR_URL` (chiefly so tests can point the
/// sender at a local listener).
const DEFAULT_COLLECTOR_URL: &str = "https://creax.de:44044/v1/ingest/hardware-reliability";
/// Default send cadence: rare batching (doc Section 6), well inside the recommended 6-24h band.
const DEFAULT_SEND_INTERVAL_SECS: u64 = 12 * 60 * 60;
const SEND_INTERVAL_MIN_SECS: u64 = 6 * 60 * 60;
const SEND_INTERVAL_MAX_SECS: u64 = 24 * 60 * 60;
/// How many past sent payloads to retain node-locally for the transparency UI (doc Section 3.3,
/// "show the last sent payload again"). Bounded so the state file cannot grow without limit.
const SENT_HISTORY_LIMIT: usize = 5;
/// Bounded retry budget per send opportunity (doc Section 6: "retried a limited number of times,
/// never queued unboundedly"). A failed batch is simply dropped until the next tick.
const SEND_MAX_ATTEMPTS: u32 = 3;
const SEND_RETRY_BACKOFF_SECS: u64 = 30;
const SEND_HTTP_TIMEOUT_SECS: u64 = 30;

/// The reduced, pseudonymized payload sent to the central fleet reliability collector (see
/// `docs/server-node-hardware-reliability-telemetry-strategy.md`, Section 7). The same
/// serialization is exposed unchanged via the preview endpoint, so operators see exactly what the
/// background sender ([`spawn_reliability_telemetry_sender`]) transmits.
///
/// Every field here is an explicit allow-listed projection of `hardware_health::HardwareHealthReport`.
/// There is deliberately no `#[serde(flatten)]` or raw passthrough of any node-local struct, so
/// that fields added to `hardware_health.rs` in the future do not silently leak into this payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReliabilityTelemetryPayload {
    schema_version: u32,
    telemetry_subject_id: String,
    generated_at_unix: u64,
    ironmesh_version: String,
    hardware_profile_id: String,
    // `country_code` is intentionally not a field here: per doc Section 4.2 it is derived
    // server-side (central collector) from the TCP source IP of the ingest request, and is
    // never computed or sent by the node itself.
    node_lifecycle: TelemetryNodeLifecycle,
    storage_devices: Vec<TelemetryStorageDevice>,
    memory_ecc: TelemetryMemoryEcc,
    reliability_findings_summary: Vec<TelemetryFindingSummary>,
    collectors: Vec<TelemetryCollectorStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryNodeLifecycle {
    uptime_seconds: Option<u64>,
    cumulative_observed_uptime_seconds: u64,
    // `boot_count_observed` from the doc Section 7 sketch is deliberately omitted: the node
    // currently only tracks a single `boot_id` string per current boot
    // (`hardware_health::HardwareNodeLifecycle::boot_id`), not a persisted count of distinct
    // boots observed over time. Deriving a real count would require a new persisted counter
    // incremented on `boot_id` change - out of scope for this slice, left for a follow-up.
}

/// Allow-listed projection of `hardware_health::HardwareStorageDevice`. Deliberately excludes
/// `component_ref` (human-readable device path alias), `block_device_name`, `vendor`, `model`,
/// `firmware_version`, `pci_slot`, `driver`, and raw serial material - none of those are copied
/// here, by construction, per doc Section 2.6.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryStorageDevice {
    component_instance_id: String,
    is_rotational: Option<bool>,
    interface_type: String,
    smart: Option<TelemetryStorageSmart>,
}

/// Allow-listed projection of `hardware_health::HardwareStorageSmartInfo` - only the SMART fields
/// sketched in doc Section 7, not the full SMART struct.
///
/// Field-by-field aggregation choice (doc Section 8's "granularity of temperature/SMART time
/// series" question, resolved here in favor of node-side aggregation): `smart_passed`,
/// `power_on_hours`, `reallocated_sector_count`, `media_errors`, and `percentage_used` are all
/// either a boolean verdict or a **monotonically non-decreasing counter** over the life of the
/// device (uptime hours only grow; reallocated sectors, media errors, and SSD wear percentage only
/// accumulate). For monotonic counters, min/max/mean over a send window adds no information beyond
/// the latest reading - the min is just the oldest sample, the max/mean are within noise of the
/// latest value - so these stay simple instantaneous "latest snapshot" values, same as before.
/// `temperature_celsius`, by contrast, genuinely fluctuates up and down with workload/ambient
/// conditions, so a single instantaneous reading is a noisy, easily-fingerprintable per-device
/// signal (doc Section 8's stated concern) - it is therefore the one field reduced node-side to
/// `{min, max, mean}` over the rolling accumulation window since the last successful send
/// (see [`TemperatureAccumulator`]), rather than sent as a raw per-sample time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryStorageSmart {
    smart_passed: Option<bool>,
    power_on_hours: Option<u64>,
    reallocated_sector_count: Option<u64>,
    media_errors: Option<u64>,
    percentage_used: Option<u64>,
    /// Lowest temperature sampled during the accumulation window. `None` if no samples were
    /// recorded (e.g. the device lacks a temperature sensor, or no `hardware_health` refresh has
    /// completed yet since the last send).
    temperature_celsius_min: Option<f32>,
    /// Highest temperature sampled during the accumulation window.
    temperature_celsius_max: Option<f32>,
    /// Arithmetic mean over all samples in the accumulation window.
    temperature_celsius_mean: Option<f32>,
}

/// RAM ECC counters projected from the node-local EDAC collector
/// (`hardware_health::HardwareMemoryEcc`, read from
/// `/sys/devices/system/edac/mc/mc*/{ce_count,ue_count}`). `available` is `false` on boards
/// without ECC RAM / a loaded EDAC driver, where the counts are `None` rather than a misleading
/// zero (doc Section 2.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryMemoryEcc {
    available: bool,
    correctable_error_count: Option<u64>,
    uncorrectable_error_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryFindingSummary {
    finding_code: String,
    occurrence_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelemetryCollectorStatus {
    collector_id: String,
    available: bool,
}

/// Rolling min/max/mean/count accumulator for one storage device's `temperature_celsius`,
/// fed by every node-local `hardware_health` refresh (every
/// `hardware_health::HARDWARE_HEALTH_REFRESH_INTERVAL_SECS`, currently 5 minutes) since the last
/// successful telemetry send, and reduced into the outbound payload's
/// `temperature_celsius_{min,max,mean}` fields (see [`TelemetryStorageSmart`]). Persisted
/// node-locally alongside the rest of `PersistedReliabilityTelemetryState` so the window survives a
/// restart across the much longer 6-24h send interval, then reset after each successful send (see
/// [`ReliabilityTelemetryRuntime::record_send_success`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct TemperatureAccumulator {
    sample_count: u64,
    sum_celsius: f64,
    min_celsius: f32,
    max_celsius: f32,
}

impl TemperatureAccumulator {
    fn record(&mut self, celsius: f32) {
        if self.sample_count == 0 {
            self.min_celsius = celsius;
            self.max_celsius = celsius;
        } else {
            self.min_celsius = self.min_celsius.min(celsius);
            self.max_celsius = self.max_celsius.max(celsius);
        }
        self.sum_celsius += f64::from(celsius);
        self.sample_count += 1;
    }

    fn mean_celsius(&self) -> f32 {
        (self.sum_celsius / self.sample_count as f64) as f32
    }
}

/// Pure converter: derives the reduced, pseudonymized telemetry payload from the existing
/// node-local `hardware_health_report`. Per doc "Open Questions" recommendation, this stays a
/// one-way projection of the existing report rather than a second independent collector, so the
/// node-local and centrally-sent views cannot drift apart.
///
/// `temperature_accumulators` is a snapshot of the rolling per-device temperature window
/// accumulated since the last successful send (keyed by `component_instance_id`), obtained via
/// [`ReliabilityTelemetryRuntime::temperature_accumulators_snapshot`]. It is passed in rather than
/// looked up internally because it lives under a different lock
/// (`ServerState::reliability_telemetry_runtime`) than the hardware-health report, and this
/// function must stay a pure, lock-free converter.
pub(crate) fn build_reliability_telemetry_payload(
    report: &hardware_health::HardwareHealthReport,
    telemetry_subject_id: String,
    generated_at_unix: u64,
    temperature_accumulators: &HashMap<String, TemperatureAccumulator>,
) -> ReliabilityTelemetryPayload {
    let storage_devices = report
        .inventory
        .storage_devices
        .iter()
        .map(|device| {
            let temperature = temperature_accumulators.get(&device.component_instance_id);
            TelemetryStorageDevice {
                component_instance_id: device.component_instance_id.clone(),
                is_rotational: device.is_rotational,
                interface_type: device.interface_type.clone(),
                smart: device.smart.as_ref().map(|smart| TelemetryStorageSmart {
                    smart_passed: smart.smart_passed,
                    power_on_hours: smart.power_on_hours,
                    reallocated_sector_count: smart.reallocated_sector_count,
                    media_errors: smart.media_errors,
                    percentage_used: smart.percentage_used,
                    temperature_celsius_min: temperature.map(|acc| acc.min_celsius),
                    temperature_celsius_max: temperature.map(|acc| acc.max_celsius),
                    temperature_celsius_mean: temperature.map(TemperatureAccumulator::mean_celsius),
                }),
            }
        })
        .collect();

    let mut finding_counts: BTreeMap<String, u64> = BTreeMap::new();
    for finding in &report.findings {
        *finding_counts
            .entry(finding.finding_code.clone())
            .or_default() += finding.occurrence_count;
    }
    let reliability_findings_summary = finding_counts
        .into_iter()
        .map(|(finding_code, occurrence_count)| TelemetryFindingSummary {
            finding_code,
            occurrence_count,
        })
        .collect();

    // Collector availability (including the real `edac` and `cpu_thermal_throttle` collectors) is
    // now sourced directly from the node-local report, so the central collector sees exactly the
    // same collector states the operator sees, per the tolerance-first convention in doc Section 7.
    let collectors: Vec<TelemetryCollectorStatus> = report
        .collectors
        .iter()
        .map(|collector| TelemetryCollectorStatus {
            collector_id: collector.collector_id.clone(),
            available: collector.available,
        })
        .collect();

    ReliabilityTelemetryPayload {
        schema_version: TELEMETRY_SCHEMA_VERSION,
        telemetry_subject_id,
        generated_at_unix,
        ironmesh_version: report.ironmesh_version.clone(),
        hardware_profile_id: report.hardware_profile_id.clone(),
        node_lifecycle: TelemetryNodeLifecycle {
            uptime_seconds: report.node_lifecycle.uptime_seconds,
            cumulative_observed_uptime_seconds: report
                .node_lifecycle
                .cumulative_observed_uptime_seconds,
        },
        storage_devices,
        memory_ecc: TelemetryMemoryEcc {
            available: report.memory_ecc.available,
            correctable_error_count: report.memory_ecc.correctable_error_count,
            uncorrectable_error_count: report.memory_ecc.uncorrectable_error_count,
        },
        reliability_findings_summary,
        collectors,
    }
}

fn compute_telemetry_subject_id(local_random_salt: &[u8], node_id: NodeId) -> String {
    let mut mac =
        HmacSha256::new_from_slice(local_random_salt).expect("HMAC accepts arbitrary key sizes");
    mac.update(TELEMETRY_HMAC_DOMAIN);
    mac.update(node_id.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

/// env var opt-out toggle, in the exact style already used for
/// `IRONMESH_AUTONOMOUS_HEARTBEAT_ENABLED` and specified verbatim in doc Section 3.1. This does
/// not reuse the crate's `env_flag_or` helper: that helper treats any non-truthy value
/// (including unset) as `false`-leaning, whereas this toggle must default to *enabled* unless
/// explicitly disabled with `"0"`/`"false"`/`"no"` - a different (inverted-default) semantic.
fn reliability_telemetry_env_enabled() -> bool {
    parse_reliability_telemetry_env_flag(
        std::env::var("IRONMESH_RELIABILITY_TELEMETRY_ENABLED").ok(),
    )
}

/// Pulled out from `reliability_telemetry_env_enabled` so tests can exercise the parsing rules
/// directly instead of mutating the real process environment (this crate denies `unsafe_code`,
/// and `std::env::set_var`/`remove_var` are `unsafe` as of the 2024 edition).
fn parse_reliability_telemetry_env_flag(value: Option<String>) -> bool {
    value
        .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

/// Central collector ingest URL, from env or the built-in default.
fn collector_url() -> String {
    std::env::var("IRONMESH_RELIABILITY_TELEMETRY_COLLECTOR_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_COLLECTOR_URL.to_string())
}

/// Effective send interval, clamped into the doc Section 6 recommended 6-24h band.
fn send_interval_secs() -> u64 {
    std::env::var("IRONMESH_RELIABILITY_TELEMETRY_SEND_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(SEND_INTERVAL_MIN_SECS, SEND_INTERVAL_MAX_SECS))
        .unwrap_or(DEFAULT_SEND_INTERVAL_SECS)
}

/// One past successful transmission, retained node-locally so the admin UI can re-show exactly
/// what left the node (doc Section 3.3). `payload` is the full serialized batch as sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SentHistoryEntry {
    sent_at_unix: u64,
    payload: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedReliabilityTelemetryState {
    /// Base64-encoded local random salt used to derive `telemetry_subject_id`
    /// (doc Section 4.1). Generated once, the first time it's needed, and never transmitted or
    /// exposed via any API response.
    local_random_salt_b64: Option<String>,
    /// `None` means "no override yet - use the env var default". `Some(_)` means an operator
    /// has explicitly set the toggle via the admin API/UI, which takes precedence over the env
    /// var going forward.
    enabled_override: Option<bool>,
    /// Unix timestamp of the last successful transmission (doc Section 3.3, "last sent at ...").
    #[serde(default)]
    last_sent_at_unix: Option<u64>,
    /// Human-readable reason the most recent send attempt failed, cleared on the next success.
    #[serde(default)]
    last_send_error: Option<String>,
    /// Content fingerprint of the last successfully sent payload (volatile fields excluded), used
    /// to skip sends when nothing material changed (doc Section 6 deduplication).
    #[serde(default)]
    last_sent_fingerprint: Option<String>,
    /// Bounded ring of the most recent sent payloads for the transparency UI.
    #[serde(default)]
    sent_history: Vec<SentHistoryEntry>,
    /// Rolling per-storage-device temperature accumulation window, keyed by
    /// `component_instance_id`, since the last successful send (doc Section 8). Reset to empty
    /// after each successful send (see `record_send_success`) so a new window starts immediately.
    #[serde(default)]
    temperature_accumulators: HashMap<String, TemperatureAccumulator>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReliabilityTelemetryRuntime {
    state_path: PathBuf,
    persisted: PersistedReliabilityTelemetryState,
}

impl ReliabilityTelemetryRuntime {
    /// Mirrors `hardware_health::HardwareHealthRuntime::load`'s load-with-graceful-fallback
    /// idiom: a missing or unparseable state file starts fresh rather than failing startup.
    pub(crate) fn load(data_dir: &FsPath) -> Self {
        let state_path = data_dir.join(RELIABILITY_TELEMETRY_STATE_FILE);
        let persisted = match fs::read_to_string(&state_path) {
            Ok(raw) => match serde_json::from_str::<PersistedReliabilityTelemetryState>(&raw) {
                Ok(parsed) => parsed,
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %state_path.display(),
                        "failed to parse persisted reliability-telemetry state; starting fresh"
                    );
                    PersistedReliabilityTelemetryState::default()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                PersistedReliabilityTelemetryState::default()
            }
            Err(err) => {
                warn!(
                    error = %err,
                    path = %state_path.display(),
                    "failed to load persisted reliability-telemetry state; starting fresh"
                );
                PersistedReliabilityTelemetryState::default()
            }
        };

        Self {
            state_path,
            persisted,
        }
    }

    /// Effective enabled state: the persisted admin override if set, else the env var default.
    pub(crate) fn effective_enabled(&self) -> bool {
        self.persisted
            .enabled_override
            .unwrap_or_else(reliability_telemetry_env_enabled)
    }

    pub(crate) fn enabled_source(&self) -> &'static str {
        if self.persisted.enabled_override.is_some() {
            "admin_override"
        } else {
            "env"
        }
    }

    pub(crate) fn last_sent_at_unix(&self) -> Option<u64> {
        self.persisted.last_sent_at_unix
    }

    pub(crate) fn last_send_error(&self) -> Option<String> {
        self.persisted.last_send_error.clone()
    }

    pub(crate) fn sent_history(&self) -> Vec<SentHistoryEntry> {
        self.persisted.sent_history.clone()
    }

    fn last_sent_fingerprint(&self) -> Option<&str> {
        self.persisted.last_sent_fingerprint.as_deref()
    }

    /// Records a successful transmission: updates the last-sent timestamp/fingerprint, clears any
    /// prior error, appends to the bounded history ring, and persists.
    async fn record_send_success(
        &mut self,
        sent_at_unix: u64,
        payload: &ReliabilityTelemetryPayload,
        fingerprint: String,
    ) -> Result<()> {
        self.persisted.last_sent_at_unix = Some(sent_at_unix);
        self.persisted.last_send_error = None;
        self.persisted.last_sent_fingerprint = Some(fingerprint);
        // Start a fresh accumulation window now that this one has been transmitted (doc Section
        // 8): the next batch's min/max/mean must reflect only samples gathered *after* this send,
        // not a window that keeps growing forever.
        self.persisted.temperature_accumulators.clear();
        let payload_json = serde_json::to_value(payload)
            .context("failed to serialize telemetry payload for sent history")?;
        self.persisted.sent_history.push(SentHistoryEntry {
            sent_at_unix,
            payload: payload_json,
        });
        let history_len = self.persisted.sent_history.len();
        if history_len > SENT_HISTORY_LIMIT {
            self.persisted
                .sent_history
                .drain(0..history_len - SENT_HISTORY_LIMIT);
        }
        self.persist().await
    }

    /// Records a failed transmission attempt (doc Section 3.3 surfaces this in the admin UI).
    async fn record_send_failure(&mut self, error: String) -> Result<()> {
        self.persisted.last_send_error = Some(error);
        self.persist().await
    }

    /// Feeds one `hardware_health` refresh sample into the rolling per-device temperature
    /// accumulator (doc Section 8). Called from every 5-minute `hardware_health` refresh cycle
    /// (`hardware_health::refresh_hardware_health_once`), independent of the much rarer telemetry
    /// send timer, so the window has accumulated many samples by the time a batch is actually
    /// built and sent. Devices no longer present in the current report have their accumulator
    /// entries pruned, so a replaced drive's stale window cannot linger indefinitely in the
    /// persisted state.
    pub(crate) async fn record_hardware_health_sample(
        &mut self,
        report: &hardware_health::HardwareHealthReport,
    ) -> Result<()> {
        let current_device_ids: HashSet<&str> = report
            .inventory
            .storage_devices
            .iter()
            .map(|device| device.component_instance_id.as_str())
            .collect();
        self.persisted
            .temperature_accumulators
            .retain(|id, _| current_device_ids.contains(id.as_str()));

        let mut changed = false;
        for device in &report.inventory.storage_devices {
            if let Some(celsius) = device
                .smart
                .as_ref()
                .and_then(|smart| smart.temperature_celsius)
            {
                self.persisted
                    .temperature_accumulators
                    .entry(device.component_instance_id.clone())
                    .or_default()
                    .record(celsius);
                changed = true;
            }
        }

        if changed {
            self.persist().await?;
        }
        Ok(())
    }

    /// Snapshot of the current per-device temperature accumulation window, for
    /// [`build_reliability_telemetry_payload`] to reduce into `{min, max, mean}` at send/preview
    /// time.
    pub(crate) fn temperature_accumulators_snapshot(
        &self,
    ) -> HashMap<String, TemperatureAccumulator> {
        self.persisted.temperature_accumulators.clone()
    }

    /// Persists (or sets to `None`, reverting to the env var default) an explicit admin
    /// override for the enabled toggle.
    pub(crate) async fn set_enabled_override(&mut self, enabled: Option<bool>) -> Result<()> {
        self.persisted.enabled_override = enabled;
        self.persist().await
    }

    /// Derives the stable `telemetry_subject_id` for this node, generating and persisting the
    /// local random salt on first use if one does not exist yet.
    pub(crate) async fn telemetry_subject_id(&mut self, node_id: NodeId) -> Result<String> {
        let salt = self.ensure_salt().await?;
        Ok(compute_telemetry_subject_id(&salt, node_id))
    }

    async fn ensure_salt(&mut self) -> Result<Vec<u8>> {
        if let Some(existing) = self.persisted.local_random_salt_b64.as_deref()
            && let Ok(bytes) = BASE64_STANDARD.decode(existing)
        {
            return Ok(bytes);
        }

        let mut salt = vec![0_u8; 32];
        rand::thread_rng().fill_bytes(&mut salt);
        self.persisted.local_random_salt_b64 = Some(BASE64_STANDARD.encode(&salt));
        self.persist().await?;
        Ok(salt)
    }

    async fn persist(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.persisted)
            .context("failed to serialize persisted reliability-telemetry state")?;
        write_json_atomic(&self.state_path, &bytes).await
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TelemetrySettingsResponse {
    enabled: bool,
    enabled_source: &'static str,
    env_default_enabled: bool,
    telemetry_subject_id: String,
    collector_url: String,
    send_interval_secs: u64,
    last_sent_at_unix: Option<u64>,
    last_send_error: Option<String>,
    sent_history: Vec<SentHistoryEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateTelemetrySettingsRequest {
    enabled: bool,
}

/// Builds the settings response from the runtime, deriving the subject id (and, on first use, its
/// salt) as a side effect. Shared by the GET and PUT handlers so they cannot drift apart.
async fn build_settings_response(
    runtime: &mut ReliabilityTelemetryRuntime,
    node_id: NodeId,
) -> Result<TelemetrySettingsResponse> {
    let telemetry_subject_id = runtime.telemetry_subject_id(node_id).await?;
    Ok(TelemetrySettingsResponse {
        enabled: runtime.effective_enabled(),
        enabled_source: runtime.enabled_source(),
        env_default_enabled: reliability_telemetry_env_enabled(),
        telemetry_subject_id,
        collector_url: collector_url(),
        send_interval_secs: send_interval_secs(),
        last_sent_at_unix: runtime.last_sent_at_unix(),
        last_send_error: runtime.last_send_error(),
        sent_history: runtime.sent_history(),
    })
}

pub(crate) async fn telemetry_settings_get(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let action = "auth/telemetry/settings/get";
    let authz = match authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        Ok(request) => request,
        Err(status) => return status.into_response(),
    };

    let mut runtime = state.reliability_telemetry_runtime.lock().await;
    let response = match build_settings_response(&mut runtime, state.node_id).await {
        Ok(response) => response,
        Err(err) => {
            warn!(error = %err, "failed to derive telemetry subject id");
            drop(runtime);
            append_admin_audit(&state, action, &authz, true, true, true, "error", json!({})).await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    drop(runtime);

    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        "success",
        json!({ "enabled": response.enabled, "source": response.enabled_source }),
    )
    .await;

    (StatusCode::OK, Json(response)).into_response()
}

pub(crate) async fn telemetry_settings_put(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<UpdateTelemetrySettingsRequest>,
) -> impl IntoResponse {
    let action = "auth/telemetry/settings/update";
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        false,
        true,
        json!({ "enabled": request.enabled }),
    )
    .await
    {
        Ok(request) => request,
        Err(status) => return status.into_response(),
    };

    let mut runtime = state.reliability_telemetry_runtime.lock().await;
    if let Err(err) = runtime.set_enabled_override(Some(request.enabled)).await {
        warn!(error = %err, "failed to persist reliability telemetry settings");
        drop(runtime);
        append_admin_audit(
            &state,
            action,
            &authz,
            true,
            false,
            true,
            "error",
            json!({}),
        )
        .await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let response = match build_settings_response(&mut runtime, state.node_id).await {
        Ok(response) => response,
        Err(err) => {
            warn!(error = %err, "failed to derive telemetry subject id");
            drop(runtime);
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                false,
                true,
                "error",
                json!({}),
            )
            .await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    drop(runtime);

    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        false,
        true,
        "success",
        json!({ "enabled": response.enabled, "source": response.enabled_source }),
    )
    .await;

    (StatusCode::OK, Json(response)).into_response()
}

/// Preview of exactly what the next batch would contain (doc Section 3.3). `payload` is `None`
/// with an `unavailable_reason` when the hardware-health report has not been collected yet, so the
/// admin UI can render a friendly "not ready" state instead of treating it as an error.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TelemetryPreviewResponse {
    payload: Option<ReliabilityTelemetryPayload>,
    unavailable_reason: Option<String>,
}

pub(crate) async fn telemetry_preview_get(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let action = "auth/telemetry/preview/get";
    let authz = match authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        Ok(request) => request,
        Err(status) => return status.into_response(),
    };

    let report = {
        let hw_runtime = state.hardware_health_runtime.lock().await;
        hw_runtime.report.clone()
    };
    let Some(report) = report else {
        append_admin_audit(
            &state,
            action,
            &authz,
            true,
            true,
            true,
            "no_report_yet",
            json!({}),
        )
        .await;
        return (
            StatusCode::OK,
            Json(TelemetryPreviewResponse {
                payload: None,
                unavailable_reason: Some(
                    "the hardware health report has not been collected yet; retry shortly"
                        .to_string(),
                ),
            }),
        )
            .into_response();
    };

    let mut runtime = state.reliability_telemetry_runtime.lock().await;
    let telemetry_subject_id = match runtime.telemetry_subject_id(state.node_id).await {
        Ok(id) => id,
        Err(err) => {
            warn!(error = %err, "failed to derive telemetry subject id");
            drop(runtime);
            append_admin_audit(&state, action, &authz, true, true, true, "error", json!({})).await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let temperature_accumulators = runtime.temperature_accumulators_snapshot();
    drop(runtime);

    let payload = build_reliability_telemetry_payload(
        &report,
        telemetry_subject_id,
        unix_ts(),
        &temperature_accumulators,
    );

    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        "success",
        json!({
            "storage_devices": payload.storage_devices.len(),
            "reliability_findings_summary": payload.reliability_findings_summary.len(),
        }),
    )
    .await;

    (
        StatusCode::OK,
        Json(TelemetryPreviewResponse {
            payload: Some(payload),
            unavailable_reason: None,
        }),
    )
        .into_response()
}

/// Content fingerprint of a payload with volatile fields (the wall-clock `generated_at_unix`)
/// excluded, so an otherwise-identical batch is recognized as "nothing material changed" and
/// skipped (doc Section 6 deduplication).
fn telemetry_payload_fingerprint(payload: &ReliabilityTelemetryPayload) -> Result<String> {
    let mut value = serde_json::to_value(payload)
        .context("failed to serialize telemetry payload for fingerprinting")?;
    if let Some(object) = value.as_object_mut() {
        object.remove("generated_at_unix");
    }
    // serde_json::Value serializes object keys in sorted order, so the string is canonical.
    let canonical = value.to_string();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Ok(hex_encode(&hasher.finalize()))
}

/// Entry point called from `hardware_health::refresh_hardware_health_once` on every 5-minute
/// `hardware_health` refresh (doc Section 8): feeds the freshly collected report into the rolling
/// per-device temperature accumulator. Kept as a free function (rather than requiring
/// `hardware_health.rs` to reach into `ReliabilityTelemetryRuntime`'s internals directly) so the
/// locking pattern for `state.reliability_telemetry_runtime` stays local to this module.
pub(crate) async fn record_hardware_health_sample(
    state: &ServerState,
    report: &hardware_health::HardwareHealthReport,
) -> Result<()> {
    let mut runtime = state.reliability_telemetry_runtime.lock().await;
    runtime.record_hardware_health_sample(report).await
}

/// Background sender (doc Section 6): on a rare timer, project the latest hardware-health report
/// into a reduced pseudonymized batch and POST it to the central collector, unless telemetry is
/// disabled or nothing material changed since the last successful send.
pub(crate) fn spawn_reliability_telemetry_sender(state: ServerState) {
    tokio::spawn(async move {
        let interval = Duration::from_secs(send_interval_secs());
        let mut ticker = tokio::time::interval(interval);
        // The first `tick()` completes immediately; consume it so we don't fire a send the instant
        // the node boots (before the first hardware-health collection has settled).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            send_reliability_telemetry_once(&state).await;
        }
    });
}

/// One send opportunity. Safe to call directly from tests. Returns whether a batch was actually
/// transmitted (vs. skipped because disabled/no-report/unchanged).
async fn send_reliability_telemetry_once(state: &ServerState) -> bool {
    {
        let runtime = state.reliability_telemetry_runtime.lock().await;
        if !runtime.effective_enabled() {
            return false;
        }
    }

    let Some(report) = ({
        let hw_runtime = state.hardware_health_runtime.lock().await;
        hw_runtime.report.clone()
    }) else {
        return false;
    };

    let (telemetry_subject_id, temperature_accumulators) = {
        let mut runtime = state.reliability_telemetry_runtime.lock().await;
        let telemetry_subject_id = match runtime.telemetry_subject_id(state.node_id).await {
            Ok(id) => id,
            Err(err) => {
                warn!(error = %err, "failed to derive telemetry subject id for send");
                return false;
            }
        };
        (
            telemetry_subject_id,
            runtime.temperature_accumulators_snapshot(),
        )
    };

    let payload = build_reliability_telemetry_payload(
        &report,
        telemetry_subject_id,
        unix_ts(),
        &temperature_accumulators,
    );
    let fingerprint = match telemetry_payload_fingerprint(&payload) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            warn!(error = %err, "failed to fingerprint telemetry payload");
            return false;
        }
    };

    {
        let runtime = state.reliability_telemetry_runtime.lock().await;
        if runtime.last_sent_fingerprint() == Some(fingerprint.as_str()) {
            // Nothing material changed since the last successful send; skip to keep baseline load
            // minimal (doc Section 6).
            return false;
        }
    }

    let url = collector_url();
    match post_telemetry_batch(&url, &payload).await {
        Ok(()) => {
            let sent_at_unix = unix_ts();
            let mut runtime = state.reliability_telemetry_runtime.lock().await;
            if let Err(err) = runtime
                .record_send_success(sent_at_unix, &payload, fingerprint)
                .await
            {
                warn!(error = %err, "failed to persist telemetry send success state");
            }
            info!(
                collector_url = %url,
                storage_devices = payload.storage_devices.len(),
                "sent reliability telemetry batch"
            );
            true
        }
        Err(err) => {
            let message = err.to_string();
            let mut runtime = state.reliability_telemetry_runtime.lock().await;
            if let Err(persist_err) = runtime.record_send_failure(message.clone()).await {
                warn!(error = %persist_err, "failed to persist telemetry send failure state");
            }
            warn!(collector_url = %url, error = %message, "reliability telemetry send failed");
            false
        }
    }
}

/// POSTs one batch with a bounded retry budget (doc Section 6: limited retries, never an unbounded
/// queue). The whole batch is dropped after the last attempt fails; the next timer tick will build
/// a fresh batch from current state rather than replaying a stale one.
async fn post_telemetry_batch(url: &str, payload: &ReliabilityTelemetryPayload) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(SEND_HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to build telemetry http client")?;

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=SEND_MAX_ATTEMPTS {
        match client
            .post(url)
            .json(payload)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_error = Some(anyhow::Error::new(err));
                if attempt < SEND_MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(SEND_RETRY_BACKOFF_SECS)).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("telemetry send failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_subject_id_is_deterministic_for_the_same_salt_and_node_id() {
        let node_id = NodeId::from_u128(42);
        let salt = b"fixed-salt-for-test".to_vec();

        let first = compute_telemetry_subject_id(&salt, node_id);
        let second = compute_telemetry_subject_id(&salt, node_id);
        assert_eq!(first, second);
        assert_eq!(
            first.len(),
            64,
            "hex-encoded SHA-256 HMAC should be 64 chars"
        );
    }

    #[test]
    fn telemetry_subject_id_differs_for_different_salts() {
        let node_id = NodeId::from_u128(42);
        let a = compute_telemetry_subject_id(b"salt-a", node_id);
        let b = compute_telemetry_subject_id(b"salt-b", node_id);
        assert_ne!(a, b);
    }

    #[test]
    fn telemetry_subject_id_differs_for_different_node_ids() {
        let salt = b"same-salt".to_vec();
        let a = compute_telemetry_subject_id(&salt, NodeId::from_u128(1));
        let b = compute_telemetry_subject_id(&salt, NodeId::from_u128(2));
        assert_ne!(a, b);
    }

    #[test]
    fn env_enabled_defaults_to_true_when_unset() {
        assert!(parse_reliability_telemetry_env_flag(None));
    }

    #[test]
    fn env_enabled_treats_falsey_strings_as_disabled() {
        for value in ["0", "false", "no"] {
            assert!(
                !parse_reliability_telemetry_env_flag(Some(value.to_string())),
                "expected {value} to disable telemetry"
            );
        }
    }

    #[test]
    fn env_enabled_treats_other_strings_as_enabled() {
        assert!(parse_reliability_telemetry_env_flag(Some("1".to_string())));
        assert!(parse_reliability_telemetry_env_flag(Some(
            "anything-else".to_string()
        )));
    }

    #[tokio::test]
    async fn persisted_state_round_trips_through_load_and_save() {
        let tmp = std::env::temp_dir().join(format!(
            "ironmesh-reliability-telemetry-test-{}",
            Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let mut runtime = ReliabilityTelemetryRuntime::load(&tmp);
        assert!(runtime.persisted.local_random_salt_b64.is_none());
        assert_eq!(runtime.enabled_source(), "env");

        let node_id = NodeId::from_u128(7);
        let subject_id_first = runtime.telemetry_subject_id(node_id).await.unwrap();
        runtime.set_enabled_override(Some(false)).await.unwrap();

        // Reload from disk and confirm both the salt (and thus subject id) and the override
        // survive a restart.
        let mut reloaded = ReliabilityTelemetryRuntime::load(&tmp);
        assert_eq!(reloaded.enabled_source(), "admin_override");
        assert!(!reloaded.effective_enabled());
        let subject_id_second = reloaded.telemetry_subject_id(node_id).await.unwrap();
        assert_eq!(subject_id_first, subject_id_second);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn converter_produces_only_allow_listed_storage_fields_and_aggregates_findings() {
        let report = hardware_health::test_support::sample_report_for_telemetry_tests();

        let payload = build_reliability_telemetry_payload(
            &report,
            "test-subject-id".to_string(),
            1_700_000_000,
            &HashMap::new(),
        );

        assert_eq!(payload.schema_version, 1);
        assert_eq!(payload.telemetry_subject_id, "test-subject-id");
        assert_eq!(payload.generated_at_unix, 1_700_000_000);

        assert_eq!(payload.storage_devices.len(), 1);
        let device = &payload.storage_devices[0];
        assert_eq!(device.interface_type, "nvme");
        assert_eq!(device.is_rotational, Some(false));
        let smart = device.smart.as_ref().expect("smart data expected");
        assert_eq!(smart.power_on_hours, Some(1234));
        assert_eq!(smart.reallocated_sector_count, Some(0));
        // No accumulated samples were passed in, so the rolling temperature aggregate must be
        // absent rather than defaulting to some misleading zero.
        assert_eq!(smart.temperature_celsius_min, None);
        assert_eq!(smart.temperature_celsius_max, None);
        assert_eq!(smart.temperature_celsius_mean, None);

        // Data-minimization: serialize and confirm no disallowed keys or raw identifiers leak
        // through, since a future field added to HardwareStorageDevice must not silently widen
        // the payload.
        let serialized = serde_json::to_value(&payload).unwrap();
        let device_json = &serialized["storage_devices"][0];
        let mut keys: Vec<&str> = device_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "component_instance_id",
                "interface_type",
                "is_rotational",
                "smart",
            ]
        );
        assert!(serialized.to_string().contains("component_instance_id"));
        assert!(!serialized.to_string().contains("component_ref"));
        assert!(!serialized.to_string().contains("block_device_name"));
        assert!(!serialized.to_string().contains("serial"));

        // Findings from two different sightings of the same finding_code must be summed, not
        // duplicated as separate entries.
        assert_eq!(payload.reliability_findings_summary.len(), 2);
        let chunk_mismatch = payload
            .reliability_findings_summary
            .iter()
            .find(|entry| entry.finding_code == "chunk_hash_mismatch")
            .expect("expected chunk_hash_mismatch summary entry");
        assert_eq!(chunk_mismatch.occurrence_count, 5);

        assert!(!payload.memory_ecc.available);
        assert!(
            payload
                .collectors
                .iter()
                .any(|collector| collector.collector_id == "edac" && !collector.available)
        );
    }

    fn sample_payload(generated_at_unix: u64) -> ReliabilityTelemetryPayload {
        let report = hardware_health::test_support::sample_report_for_telemetry_tests();
        build_reliability_telemetry_payload(
            &report,
            "subject".to_string(),
            generated_at_unix,
            &HashMap::new(),
        )
    }

    #[test]
    fn temperature_accumulator_reduces_to_min_max_mean_over_recorded_samples() {
        let mut acc = TemperatureAccumulator::default();
        acc.record(30.0);
        acc.record(40.0);
        acc.record(35.0);

        assert_eq!(acc.sample_count, 3);
        assert_eq!(acc.min_celsius, 30.0);
        assert_eq!(acc.max_celsius, 40.0);
        assert_eq!(acc.mean_celsius(), 35.0);
    }

    #[test]
    fn converter_reduces_accumulated_temperature_samples_into_min_max_mean() {
        let report = hardware_health::test_support::sample_report_for_telemetry_tests();
        let component_instance_id = report.inventory.storage_devices[0]
            .component_instance_id
            .clone();

        let mut acc = TemperatureAccumulator::default();
        for celsius in [30.0, 45.0, 33.0] {
            acc.record(celsius);
        }
        let mut accumulators = HashMap::new();
        accumulators.insert(component_instance_id, acc);

        let payload = build_reliability_telemetry_payload(
            &report,
            "subject".to_string(),
            1_700_000_000,
            &accumulators,
        );

        let smart = payload.storage_devices[0]
            .smart
            .as_ref()
            .expect("smart data expected");
        assert_eq!(smart.temperature_celsius_min, Some(30.0));
        assert_eq!(smart.temperature_celsius_max, Some(45.0));
        assert_eq!(smart.temperature_celsius_mean, Some(36.0));
        // Monotonic counters must stay simple instantaneous latest-value passthroughs, unaffected
        // by the presence of a temperature accumulator.
        assert_eq!(smart.power_on_hours, Some(1234));
        assert_eq!(smart.reallocated_sector_count, Some(0));
    }

    #[tokio::test]
    async fn record_hardware_health_sample_accumulates_across_calls_and_persists() {
        let tmp = std::env::temp_dir().join(format!(
            "ironmesh-reliability-telemetry-accum-test-{}",
            Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let mut runtime = ReliabilityTelemetryRuntime::load(&tmp);
        let report = hardware_health::test_support::sample_report_for_telemetry_tests();
        let component_instance_id = report.inventory.storage_devices[0]
            .component_instance_id
            .clone();

        runtime
            .record_hardware_health_sample(&report)
            .await
            .unwrap();
        runtime
            .record_hardware_health_sample(&report)
            .await
            .unwrap();

        let snapshot = runtime.temperature_accumulators_snapshot();
        let acc = snapshot
            .get(&component_instance_id)
            .expect("expected an accumulator entry for the sampled device");
        assert_eq!(acc.sample_count, 2);
        // Both samples came from the same fixture report (35.0 C), so min/max/mean coincide.
        assert_eq!(acc.min_celsius, 35.0);
        assert_eq!(acc.max_celsius, 35.0);
        assert_eq!(acc.mean_celsius(), 35.0);

        // Reload from disk: the accumulation window must survive a restart, since it is meant to
        // span the multi-hour send interval reliably.
        let reloaded = ReliabilityTelemetryRuntime::load(&tmp);
        let reloaded_snapshot = reloaded.temperature_accumulators_snapshot();
        let reloaded_acc = reloaded_snapshot
            .get(&component_instance_id)
            .expect("expected the accumulator to survive a reload");
        assert_eq!(reloaded_acc.sample_count, 2);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn record_send_success_resets_the_temperature_accumulation_window() {
        let tmp = std::env::temp_dir().join(format!(
            "ironmesh-reliability-telemetry-reset-test-{}",
            Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let mut runtime = ReliabilityTelemetryRuntime::load(&tmp);
        let report = hardware_health::test_support::sample_report_for_telemetry_tests();
        runtime
            .record_hardware_health_sample(&report)
            .await
            .unwrap();
        assert!(!runtime.temperature_accumulators_snapshot().is_empty());

        let payload = build_reliability_telemetry_payload(
            &report,
            "subject".to_string(),
            1_700_000_000,
            &runtime.temperature_accumulators_snapshot(),
        );
        let fingerprint = telemetry_payload_fingerprint(&payload).unwrap();
        runtime
            .record_send_success(1_700_000_000, &payload, fingerprint)
            .await
            .unwrap();

        assert!(
            runtime.temperature_accumulators_snapshot().is_empty(),
            "a successful send must start a fresh accumulation window"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn fingerprint_ignores_generated_at_but_tracks_content_changes() {
        let a = telemetry_payload_fingerprint(&sample_payload(1_000)).unwrap();
        let b = telemetry_payload_fingerprint(&sample_payload(2_000)).unwrap();
        assert_eq!(
            a, b,
            "differing only in generated_at_unix must not change fingerprint"
        );

        let mut changed = sample_payload(1_000);
        changed.hardware_profile_id = "different-profile".to_string();
        let c = telemetry_payload_fingerprint(&changed).unwrap();
        assert_ne!(
            a, c,
            "a material content change must change the fingerprint"
        );
    }

    #[tokio::test]
    async fn post_telemetry_batch_succeeds_against_a_collector_returning_202() {
        use axum::Json;
        use axum::routing::post;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_route = hits.clone();
        let app = axum::Router::new().route(
            "/v1/ingest/hardware-reliability",
            post(move |Json(body): Json<serde_json::Value>| {
                let hits = hits_for_route.clone();
                async move {
                    // The collector's plausibility contract: object with schema_version + subject.
                    assert_eq!(body["schema_version"], 1);
                    assert!(body["telemetry_subject_id"].is_string());
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/v1/ingest/hardware-reliability");
        post_telemetry_batch(&url, &sample_payload(1_000))
            .await
            .expect("send should succeed against a 202 collector");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
