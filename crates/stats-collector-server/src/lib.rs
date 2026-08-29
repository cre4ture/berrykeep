//! `stats-collector-server` — the central hardware-reliability telemetry ingestion service.
//!
//! See `docs/server-node-hardware-reliability-telemetry-strategy.md` for the full design; this
//! crate implements Sections 5-7: tolerant ingestion, append-only raw storage, k-anonymity-safe
//! public aggregates ([`aggregate`], Section 4.3), admin-token-guarded GDPR access/erasure
//! (Section 4.5), and a retention sweeper (Section 4.6).
//!
//! This is a deliberately small, standalone service (Section 5.1/5.4): unlike
//! `rendezvous-server`, it has **no per-node identity or mTLS** — the entire point is that the
//! collector cannot tell which cluster/operator a given record came from (Section 5.2). Abuse
//! protection is via rate limiting (see [`rate_limit`]) rather than authentication.
//!
//! A real, offline IP-to-country [`country::CountryResolver`] is available opt-in via the
//! `bundled-country-db` Cargo feature ([`country::BundledCountryResolver`]); the default
//! ([`country::NoopCountryResolver`]) resolves nothing, so no extra dependency is pulled in unless
//! a deployment opts in (Section 4.2).
//!
//! Left for later work (seams are in place):
//! - moving the on-request aggregation to a periodic batch job if the fleet ever outgrows it,
//! - packaging, process supervision, backup, and monitoring beyond the deterministic deployment
//!   helper in `scripts/deploy-stats-collector-service.sh`. The binary supports direct TLS through
//!   environment-provided certificate/key paths and reloads renewed certificates without restart.

pub mod aggregate;
pub mod country;
pub mod ingest;
pub mod rate_limit;
pub mod registration;
pub mod storage;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::services::ServeDir;
use tracing::{info, warn};

use crate::aggregate::{FleetSummary, summarize};
use crate::country::{CountryResolver, NoopCountryResolver};
use crate::ingest::{PayloadValidationError, validate_payload};
use crate::rate_limit::SlidingWindowLimiter;
use crate::registration::{
    RegistrationError, generate_ingestion_token, validate_subject_id_for_registration,
};
use crate::storage::{IngestStorage, StoredRecord};

const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Admin authentication header, matching the server-node admin plane convention
/// (`x-ironmesh-admin-token`). See doc Section 5.3.
pub const ADMIN_TOKEN_HEADER: &str = "x-ironmesh-admin-token";

/// Ingestion token header (doc Section 5.2/8): an optional, opaque per-`telemetry_subject_id`
/// credential issued by `POST /v1/register/{telemetry_subject_id}` and presented on subsequent
/// ingestion requests. Deliberately a header rather than a payload field, so it never ends up
/// inside the stored `raw_payload_json` blob (Section 2.6's "don't let auxiliary material leak
/// into stored payloads" spirit) and so the node side can attach it without touching payload
/// construction at all.
pub const INGESTION_TOKEN_HEADER: &str = "x-ironmesh-ingestion-token";

/// Default k-anonymity minimum group size for published aggregates (doc Section 4.3).
pub const DEFAULT_K_ANONYMITY_MIN: u32 = 5;

/// Default raw-data retention window before pruning (doc Section 4.6).
pub const DEFAULT_RETENTION_DAYS: u64 = 180;

/// Per-source-IP limit: generous enough that a handful of nodes sharing one NAT'd IP (a small
/// office/homelab cluster) don't trip it, but low enough to blunt a single-source flood, given
/// the expected send cadence is one batch every 6-24h per node (Section 6).
pub const RATE_LIMIT_PER_IP_MAX_REQUESTS: u32 = 20;
pub const RATE_LIMIT_PER_IP_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Per-`telemetry_subject_id` limit: a well-behaved node sends at most a couple of batches per
/// hour even in the worst case (e.g. retries after failures, Section 6), so this stays tight.
pub const RATE_LIMIT_PER_SUBJECT_MAX_REQUESTS: u32 = 4;
pub const RATE_LIMIT_PER_SUBJECT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Dedicated, tighter per-IP limit for `POST /v1/register/*` on top of the shared `ip_limiter`
/// (doc Section 5.2/8). Registration is a "mint a credential" endpoint: unlike ingestion (which is
/// self-limiting by the expected once-per-6-24h send cadence and by requiring a plausible payload
/// body), minting a token only requires a cheap path parameter, and for a well-behaved node is
/// normally a rare one-time event (first activation, or a retry if the first attempt failed before
/// persisting the token). A low ceiling here blunts credential-minting spam without affecting any
/// legitimate node, while still keeping the endpoint covered by the same shared `ip_limiter` used
/// for ingestion (never an unbounded new attack surface).
pub const RATE_LIMIT_REGISTER_PER_IP_MAX_REQUESTS: u32 = 5;
pub const RATE_LIMIT_REGISTER_PER_IP_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Stricter per-`telemetry_subject_id` ceiling applied *in addition to* `subject_limiter` for
/// ingestion requests that present no ingestion token at all (doc Section 8's tolerance-first
/// policy: tokenless requests are still accepted, since older/non-upgraded nodes have nothing to
/// present yet, but they get less headroom than tokened traffic as a mild incentive to register).
/// This is strictly additive: it never replaces or loosens `ip_limiter`/`subject_limiter`, which
/// apply identically regardless of whether a token is presented.
pub const RATE_LIMIT_UNTOKENED_PER_SUBJECT_MAX_REQUESTS: u32 = 2;
pub const RATE_LIMIT_UNTOKENED_PER_SUBJECT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Default local bind address, overridable via `STATS_COLLECTOR_BIND_ADDR`.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:44044";

/// Default Turso (embedded, SQLite-compatible) database path, overridable via
/// `STATS_COLLECTOR_DB_PATH`.
pub const DEFAULT_DB_PATH: &str = "stats-collector.sqlite3";

#[derive(Clone)]
pub struct StatsCollectorAppState {
    storage: Arc<IngestStorage>,
    ip_limiter: Arc<SlidingWindowLimiter>,
    subject_limiter: Arc<SlidingWindowLimiter>,
    /// Dedicated, tighter per-IP limiter for the registration ("mint a credential") endpoint, on
    /// top of the shared `ip_limiter` (doc Section 5.2/8). See
    /// `RATE_LIMIT_REGISTER_PER_IP_MAX_REQUESTS` for the reasoning.
    register_ip_limiter: Arc<SlidingWindowLimiter>,
    /// Stricter per-subject limiter applied only to tokenless ingestion requests, in addition to
    /// `subject_limiter` (doc Section 8). See `RATE_LIMIT_UNTOKENED_PER_SUBJECT_MAX_REQUESTS`.
    untokened_subject_limiter: Arc<SlidingWindowLimiter>,
    /// Admin bearer token for the raw-access / erasure endpoints (doc Sections 4.5, 5.3). `None`
    /// means "not configured", in which case those endpoints return 412 rather than operating
    /// unauthenticated.
    admin_token: Option<Arc<String>>,
    /// Minimum distinct-subject group size for published aggregates (doc Section 4.3).
    k_anonymity_min: u32,
    /// Server-side country-code derivation (doc Section 4.2); the default resolves nothing.
    country_resolver: Arc<dyn CountryResolver>,
}

impl StatsCollectorAppState {
    /// Builds app state with the default (documented) rate limits.
    pub fn new(storage: IngestStorage) -> Self {
        Self::with_rate_limits(
            storage,
            RATE_LIMIT_PER_IP_MAX_REQUESTS,
            RATE_LIMIT_PER_IP_WINDOW,
            RATE_LIMIT_PER_SUBJECT_MAX_REQUESTS,
            RATE_LIMIT_PER_SUBJECT_WINDOW,
        )
    }

    /// Builds app state with custom ingestion rate limits, primarily so tests can use tight
    /// limits without waiting out a real window. The registration/untokened-ingestion limiters get
    /// the default (documented) values; use `with_register_rate_limit` /
    /// `with_untokened_subject_rate_limit` to override those too. Admin token unset, default
    /// k-anonymity, no-op country resolver; use the `with_*` builders to override.
    pub fn with_rate_limits(
        storage: IngestStorage,
        ip_max_requests: u32,
        ip_window: Duration,
        subject_max_requests: u32,
        subject_window: Duration,
    ) -> Self {
        Self {
            storage: Arc::new(storage),
            ip_limiter: Arc::new(SlidingWindowLimiter::new(ip_max_requests, ip_window)),
            subject_limiter: Arc::new(SlidingWindowLimiter::new(
                subject_max_requests,
                subject_window,
            )),
            register_ip_limiter: Arc::new(SlidingWindowLimiter::new(
                RATE_LIMIT_REGISTER_PER_IP_MAX_REQUESTS,
                RATE_LIMIT_REGISTER_PER_IP_WINDOW,
            )),
            untokened_subject_limiter: Arc::new(SlidingWindowLimiter::new(
                RATE_LIMIT_UNTOKENED_PER_SUBJECT_MAX_REQUESTS,
                RATE_LIMIT_UNTOKENED_PER_SUBJECT_WINDOW,
            )),
            admin_token: None,
            k_anonymity_min: DEFAULT_K_ANONYMITY_MIN,
            country_resolver: Arc::new(NoopCountryResolver),
        }
    }

    /// Overrides the dedicated registration-endpoint rate limit (see
    /// `RATE_LIMIT_REGISTER_PER_IP_MAX_REQUESTS`), primarily so tests can use tight limits.
    pub fn with_register_rate_limit(mut self, max_requests: u32, window: Duration) -> Self {
        self.register_ip_limiter = Arc::new(SlidingWindowLimiter::new(max_requests, window));
        self
    }

    /// Overrides the tokenless-ingestion per-subject rate limit (see
    /// `RATE_LIMIT_UNTOKENED_PER_SUBJECT_MAX_REQUESTS`), primarily so tests can use tight limits.
    pub fn with_untokened_subject_rate_limit(
        mut self,
        max_requests: u32,
        window: Duration,
    ) -> Self {
        self.untokened_subject_limiter = Arc::new(SlidingWindowLimiter::new(max_requests, window));
        self
    }

    /// Sets the admin token (empty/whitespace is treated as unset).
    pub fn with_admin_token(mut self, admin_token: Option<String>) -> Self {
        self.admin_token = admin_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .map(Arc::new);
        self
    }

    /// Overrides the k-anonymity minimum group size.
    pub fn with_k_anonymity_min(mut self, k_anonymity_min: u32) -> Self {
        self.k_anonymity_min = k_anonymity_min.max(1);
        self
    }

    /// Plugs in a country resolver (e.g. a GeoIP-backed one in production, doc Section 4.2).
    pub fn with_country_resolver(mut self, resolver: Arc<dyn CountryResolver>) -> Self {
        self.country_resolver = resolver;
        self
    }

    /// Runs one sweep of stale rate-limit bookkeeping. Intended to be called periodically (see
    /// `main.rs`) so long-running processes don't accumulate unbounded per-key state for callers
    /// that never come back.
    pub fn cleanup_rate_limiters(&self) {
        self.ip_limiter.cleanup_stale_entries();
        self.subject_limiter.cleanup_stale_entries();
        self.register_ip_limiter.cleanup_stale_entries();
        self.untokened_subject_limiter.cleanup_stale_entries();
    }

    /// Exposes the underlying storage.
    pub fn storage(&self) -> &IngestStorage {
        &self.storage
    }

    /// Deletes raw rows older than `retention_days` (doc Section 4.6). Returns the number pruned.
    /// Intended to be called periodically (see `main.rs`).
    pub async fn prune_expired(&self, retention_days: u64) -> anyhow::Result<usize> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let cutoff = now.saturating_sub((retention_days.saturating_mul(24 * 60 * 60)) as i64);
        self.storage.delete_older_than(cutoff).await
    }
}

/// Builds the API-only axum [`Router`] for this service. Callers are responsible for TLS
/// termination and for actually binding/serving it (see `main.rs`).
///
/// Production deployments normally use [`build_router_with_public_assets`] so the public
/// dashboard and its API share the same HTTPS origin. Keeping the API-only constructor also makes
/// small integration tests and API-only deployments straightforward.
pub fn build_router(state: StatsCollectorAppState) -> Router {
    build_router_with_public_assets(state, None)
}

/// Builds the collector router, optionally serving a compiled public dashboard as its fallback.
///
/// The dashboard contains no credentials and calls only the k-anonymized public statistics API.
/// Serving it from the collector's HTTPS origin avoids an unnecessarily broad CORS policy. The
/// caller must provide a directory containing a Vite build, including `index.html` and `assets/`.
pub fn build_router_with_public_assets(
    state: StatsCollectorAppState,
    public_assets_dir: Option<PathBuf>,
) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route(
            "/v1/ingest/hardware-reliability",
            post(ingest_hardware_reliability),
        )
        // Registration handshake for the optional ingestion token (doc Section 5.2/8).
        .route(
            "/v1/register/{telemetry_subject_id}",
            post(register_ingestion_token),
        )
        // Public, k-anonymity-safe fleet statistics (doc Sections 4.3, 5.3).
        .route("/v1/stats/summary", get(stats_summary))
        .route("/v1/stats/dashboard", get(stats_dashboard))
        // Admin-token-protected GDPR access + erasure (doc Section 4.5).
        .route("/v1/admin/raw", get(admin_raw_records))
        .route(
            "/v1/admin/subject/{telemetry_subject_id}",
            delete(admin_delete_subject),
        )
        .with_state(state);

    match public_assets_dir {
        Some(dir) => {
            router.fallback_service(ServeDir::new(dir).append_index_html_on_directories(true))
        }
        None => router,
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    software_version: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        software_version: PACKAGE_VERSION,
    })
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for PayloadValidationError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: self.message(),
            }),
        )
            .into_response()
    }
}

impl IntoResponse for RegistrationError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: self.message(),
            }),
        )
            .into_response()
    }
}

struct RateLimited;

impl IntoResponse for RateLimited {
    fn into_response(self) -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "rate limit exceeded".to_string(),
            }),
        )
            .into_response()
    }
}

async fn ingest_hardware_reliability(
    State(state): State<StatsCollectorAppState>,
    ConnectInfo(source_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let validated = match validate_payload(&payload) {
        Ok(validated) => validated,
        Err(error) => return error.into_response(),
    };

    // The source IP is used only transiently, in-memory, as a rate-limit key — it is never
    // logged, stored, or forwarded (see `storage.rs` module docs and Section 2.6). These two
    // checks apply identically regardless of whether an ingestion token is presented below - the
    // token is an additional, independent layer on top, never a substitute for this baseline.
    if !state
        .ip_limiter
        .check_and_record(&source_addr.ip().to_string())
    {
        return RateLimited.into_response();
    }
    if !state
        .subject_limiter
        .check_and_record(&validated.telemetry_subject_id)
    {
        return RateLimited.into_response();
    }

    // Ingestion token policy (doc Section 5.2/8, resolving the "abuse protection without
    // identity" open question):
    // - A token that doesn't match what's on file for this `telemetry_subject_id` (including the
    //   case where nothing is on file at all) is a forgery/spoofing signal and is rejected
    //   outright with 401, rather than silently downgraded to "treat as tokenless".
    // - No token at all is still *accepted* - older/not-yet-upgraded nodes have nothing to
    //   present yet, and the doc's tolerance-first schema-evolution philosophy (Section 7) argues
    //   against a hard cutover on a brand new, optional credential. Tokenless requests are merely
    //   tracked (logged) and subject to a stricter, dedicated per-subject ceiling
    //   (`untokened_subject_limiter`) as a mild incentive to register, without breaking anything
    //   for nodes that haven't upgraded.
    let provided_token = headers
        .get(INGESTION_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match provided_token {
        Some(provided) => {
            let expected = match state
                .storage
                .token_for_subject(&validated.telemetry_subject_id)
                .await
            {
                Ok(expected) => expected,
                Err(error) => {
                    warn!(%error, "failed to look up ingestion token");
                    return internal_error("failed to validate ingestion token");
                }
            };
            let matches = expected
                .as_deref()
                .is_some_and(|expected| constant_time_eq(expected.as_bytes(), provided.as_bytes()));
            if !matches {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse {
                        error: "ingestion token does not match the registered token for this \
                                telemetry_subject_id"
                            .to_string(),
                    }),
                )
                    .into_response();
            }
        }
        None => {
            info!(
                subject = %validated.telemetry_subject_id,
                "accepted hardware-reliability ingestion without an ingestion token"
            );
            if !state
                .untokened_subject_limiter
                .check_and_record(&validated.telemetry_subject_id)
            {
                return RateLimited.into_response();
            }
        }
    }

    let received_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Derive the coarse country code from the source IP, then let the IP go out of scope — it is
    // never stored or logged (doc Sections 4.2, 2.6). Any `country_code` in the payload body is
    // ignored; only this server-derived value is trusted.
    let country_code = state.country_resolver.resolve(source_addr.ip());

    let raw_payload_json = payload.to_string();
    let insert_result = state
        .storage
        .insert(
            received_at_unix,
            &validated.telemetry_subject_id,
            validated.schema_version,
            country_code.as_deref(),
            &raw_payload_json,
        )
        .await;

    if let Err(error) = insert_result {
        warn!(%error, "failed to persist hardware-reliability ingestion record");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "failed to persist payload".to_string(),
            }),
        )
            .into_response();
    }

    StatusCode::ACCEPTED.into_response()
}

#[derive(Debug, Serialize)]
struct RegisterResponse {
    telemetry_subject_id: String,
    token: String,
}

/// `POST /v1/register/{telemetry_subject_id}` (doc Section 5.2/8): issues, idempotently, an opaque
/// ingestion token scoped to one pseudonymous `telemetry_subject_id`. This resolves the Section 8
/// "abuse protection without identity" open question: the token proves only "this caller
/// previously completed this registration handshake for this subject id" - it carries no
/// real-world identity, is never joined into any admin/raw-record view (see `storage.rs`), and its
/// presence/absence never affects the *existing* IP/subject rate limits applied at ingestion
/// (`ip_limiter`/`subject_limiter` run identically either way) - it is an independent, additive
/// signal layered on top, not a replacement.
///
/// Idempotency: calling this again for a subject id that already has a token returns the *same*
/// token rather than minting a new one, so an operator re-running setup, or a node retrying after
/// a crash before it persisted the response, cannot accidentally invalidate its own credential
/// (see `IngestStorage::get_or_create_ingestion_token`).
///
/// Rate limiting: this endpoint is covered by the same shared `ip_limiter` used for ingestion (so
/// it never becomes an unbounded new attack surface), *plus* a dedicated, tighter
/// `register_ip_limiter` - minting a credential is far cheaper per request than a full
/// plausibility-checked ingestion batch, and unlike ingestion is not expected to recur often for a
/// well-behaved node, so a low ceiling here specifically blunts token-minting spam.
async fn register_ingestion_token(
    State(state): State<StatsCollectorAppState>,
    ConnectInfo(source_addr): ConnectInfo<SocketAddr>,
    Path(telemetry_subject_id): Path<String>,
) -> Response {
    let telemetry_subject_id = match validate_subject_id_for_registration(&telemetry_subject_id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };

    let ip_key = source_addr.ip().to_string();
    if !state.ip_limiter.check_and_record(&ip_key) {
        return RateLimited.into_response();
    }
    if !state.register_ip_limiter.check_and_record(&ip_key) {
        return RateLimited.into_response();
    }

    let created_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let candidate_token = generate_ingestion_token();
    match state
        .storage
        .get_or_create_ingestion_token(&telemetry_subject_id, created_at_unix, &candidate_token)
        .await
    {
        Ok(token) => (
            StatusCode::OK,
            Json(RegisterResponse {
                telemetry_subject_id,
                token,
            }),
        )
            .into_response(),
        Err(error) => {
            warn!(%error, "failed to issue/read ingestion token");
            internal_error("failed to issue ingestion token")
        }
    }
}

fn internal_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// Public, k-anonymity-safe fleet statistics (doc Sections 4.3, 5.3). Computed on request; the
/// small target fleet makes this cheap, and the code is structured so a periodic batch job could
/// replace the on-request computation later without changing the response shape.
async fn stats_summary(State(state): State<StatsCollectorAppState>) -> Response {
    public_summary_response(state, false).await
}

/// Versioned shape consumed by the public dashboard. It deliberately only includes values already
/// approved for public release by [`summarize`]; there are no raw batches, subject identifiers, or
/// administrative fields in this response.
#[derive(Debug, Serialize)]
struct FleetDashboardResponse {
    schema_version: u32,
    generated_at_unix: i64,
    software_version: &'static str,
    #[serde(flatten)]
    summary: FleetSummary,
}

/// Public dashboard data, computed on request from the same k-anonymized aggregate used by the
/// compatibility summary endpoint. Its cache policy is deliberately short: normal viewers do not
/// trigger a database read for every page view, while new daily telemetry remains visible promptly.
async fn stats_dashboard(State(state): State<StatsCollectorAppState>) -> Response {
    public_summary_response(state, true).await
}

async fn public_summary_response(state: StatsCollectorAppState, dashboard: bool) -> Response {
    match state.storage.all_records().await {
        Ok(records) => {
            let summary: FleetSummary = summarize(&records, state.k_anonymity_min);
            let mut response = if dashboard {
                (
                    StatusCode::OK,
                    Json(FleetDashboardResponse {
                        schema_version: 1,
                        generated_at_unix: now_unix(),
                        software_version: PACKAGE_VERSION,
                        summary,
                    }),
                )
                    .into_response()
            } else {
                (StatusCode::OK, Json(summary)).into_response()
            };
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=300, stale-while-revalidate=600"),
            );
            response
        }
        Err(error) => {
            warn!(%error, "failed to compute public fleet summary");
            internal_error("failed to compute summary")
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Authorizes an admin request against the configured token. Returns 412 when no token is
/// configured (mirrors the server-node "denied_unconfigured" stance) and 401 on missing/mismatched
/// tokens. The comparison is constant-time.
fn authorize_admin(state: &StatsCollectorAppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(expected) = state.admin_token.as_ref() else {
        return Err(StatusCode::PRECONDITION_FAILED);
    };
    let provided = headers
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Deserialize)]
struct RawQuery {
    telemetry_subject_id: String,
}

#[derive(Debug, Serialize)]
struct RawRecordsResponse {
    telemetry_subject_id: String,
    records: Vec<RawRecordView>,
}

#[derive(Debug, Serialize)]
struct RawRecordView {
    received_at_unix: i64,
    schema_version: u32,
    country_code: Option<String>,
    payload: Value,
}

fn raw_record_view(record: StoredRecord) -> RawRecordView {
    RawRecordView {
        received_at_unix: record.received_at_unix,
        schema_version: record.schema_version,
        country_code: record.country_code,
        payload: serde_json::from_str(&record.raw_payload_json).unwrap_or(Value::Null),
    }
}

/// GDPR access (doc Section 4.5): returns all raw records for a `telemetry_subject_id`. The subject
/// id itself is the only credential needed to inspect "your" data, since it is not personally
/// identifying and cannot be mapped back to a node without the node's local salt.
async fn admin_raw_records(
    State(state): State<StatsCollectorAppState>,
    headers: HeaderMap,
    Query(query): Query<RawQuery>,
) -> Response {
    if let Err(status) = authorize_admin(&state, &headers) {
        return status.into_response();
    }
    match state
        .storage
        .records_for_subject(&query.telemetry_subject_id)
        .await
    {
        Ok(records) => {
            let records = records.into_iter().map(raw_record_view).collect();
            (
                StatusCode::OK,
                Json(RawRecordsResponse {
                    telemetry_subject_id: query.telemetry_subject_id,
                    records,
                }),
            )
                .into_response()
        }
        Err(error) => {
            warn!(%error, "failed to read raw records for subject");
            internal_error("failed to read records")
        }
    }
}

#[derive(Debug, Serialize)]
struct DeleteSubjectResponse {
    telemetry_subject_id: String,
    deleted_records: usize,
}

/// GDPR erasure (doc Section 4.5): deletes all raw records for a `telemetry_subject_id`. Aggregated
/// k-anonymous statistics already published do not need retroactive correction (standard practice
/// for aggregates).
async fn admin_delete_subject(
    State(state): State<StatsCollectorAppState>,
    headers: HeaderMap,
    Path(telemetry_subject_id): Path<String>,
) -> Response {
    if let Err(status) = authorize_admin(&state, &headers) {
        return status.into_response();
    }
    match state.storage.delete_subject(&telemetry_subject_id).await {
        Ok(deleted_records) => {
            info!(deleted_records, "erased telemetry subject on request");
            (
                StatusCode::OK,
                Json(DeleteSubjectResponse {
                    telemetry_subject_id,
                    deleted_records,
                }),
            )
                .into_response()
        }
        Err(error) => {
            warn!(%error, "failed to erase telemetry subject");
            internal_error("failed to erase subject")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::connect_info::ConnectInfo;
    use axum::http::{Request, header};
    use serde_json::json;
    use tower::ServiceExt;

    async fn test_state() -> StatsCollectorAppState {
        StatsCollectorAppState::with_rate_limits(
            IngestStorage::open_in_memory()
                .await
                .expect("storage should open"),
            RATE_LIMIT_PER_IP_MAX_REQUESTS,
            RATE_LIMIT_PER_IP_WINDOW,
            RATE_LIMIT_PER_SUBJECT_MAX_REQUESTS,
            RATE_LIMIT_PER_SUBJECT_WINDOW,
        )
    }

    fn source_addr(ip_suffix: u8) -> SocketAddr {
        format!("203.0.113.{ip_suffix}:51000")
            .parse()
            .expect("test source addr should parse")
    }

    fn ingest_request(body: Value, addr: SocketAddr) -> Request<Body> {
        ingest_request_with_token(body, addr, None)
    }

    fn ingest_request_with_token(
        body: Value,
        addr: SocketAddr,
        token: Option<&str>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/ingest/hardware-reliability")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(addr));
        if let Some(token) = token {
            builder = builder.header(INGESTION_TOKEN_HEADER, token);
        }
        builder
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .expect("request should build")
    }

    fn register_request(telemetry_subject_id: &str, addr: SocketAddr) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/v1/register/{telemetry_subject_id}"))
            .extension(ConnectInfo(addr))
            .body(Body::empty())
            .expect("request should build")
    }

    #[tokio::test]
    async fn health_route_reports_ok() {
        let router = build_router(test_state().await);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_asset_fallback_serves_the_dashboard_index() {
        let assets = tempfile::tempdir().expect("temporary dashboard asset directory should open");
        std::fs::write(
            assets.path().join("index.html"),
            "<!doctype html><title>IronMesh Fleet Reliability</title>",
        )
        .expect("dashboard index should be written");
        let router =
            build_router_with_public_assets(test_state().await, Some(assets.path().to_path_buf()));

        let response = router
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("dashboard response body should read");
        assert!(String::from_utf8_lossy(&body).contains("IronMesh Fleet Reliability"));
    }

    #[tokio::test]
    async fn happy_path_ingest_then_row_exists() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-happy-path",
            "generated_at_unix": 1_752_912_000_u64,
            "ironmesh_version": "1.0.35",
            "hardware_profile_id": "hp-abc",
            "country_code": "DE",
            "node_lifecycle": {"uptime_seconds": 100},
            "storage_devices": [],
            "memory_ecc": {"available": true},
            "reliability_findings_summary": [],
            "collectors": [{"collector_id": "smartctl", "available": true}],
        });

        let response = router
            .oneshot(ingest_request(payload, source_addr(1)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let records = state
            .storage
            .records_for_subject("subject-happy-path")
            .await
            .expect("query should succeed");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].schema_version, 1);
        // Even though the payload above included a country_code, it must never be trusted:
        // this slice always stores NULL until real geo-IP derivation exists (Section 4.2).
        assert_eq!(records[0].country_code, None);
    }

    #[tokio::test]
    async fn tolerates_unknown_top_level_fields() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-tolerant",
            "a_field_from_the_future": {"whatever": [1, 2, 3]},
        });

        let response = router
            .oneshot(ingest_request(payload, source_addr(2)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            state.storage.count().await.expect("count should succeed"),
            1
        );
    }

    #[tokio::test]
    async fn rejects_unsupported_schema_version() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let payload = json!({
            "schema_version": 2,
            "telemetry_subject_id": "subject-bad-version",
        });

        let response = router
            .oneshot(ingest_request(payload, source_addr(3)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            state.storage.count().await.expect("count should succeed"),
            0
        );
    }

    #[tokio::test]
    async fn rejects_missing_telemetry_subject_id() {
        let router = build_router(test_state().await);
        let payload = json!({ "schema_version": 1 });

        let response = router
            .oneshot(ingest_request(payload, source_addr(4)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rate_limits_after_n_requests_from_same_subject() {
        let state = StatsCollectorAppState::with_rate_limits(
            IngestStorage::open_in_memory()
                .await
                .expect("storage should open"),
            RATE_LIMIT_PER_IP_MAX_REQUESTS,
            RATE_LIMIT_PER_IP_WINDOW,
            2,
            Duration::from_secs(60 * 60),
        );
        let router = build_router(state.clone());
        let addr = source_addr(5);

        for _ in 0..2 {
            let payload = json!({
                "schema_version": 1,
                "telemetry_subject_id": "subject-rate-limited",
            });
            let response = router
                .clone()
                .oneshot(ingest_request(payload, addr))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-rate-limited",
        });
        let response = router
            .oneshot(ingest_request(payload, addr))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            state.storage.count().await.expect("count should succeed"),
            2
        );
    }

    #[tokio::test]
    async fn rate_limits_after_n_requests_from_same_ip() {
        let state = StatsCollectorAppState::with_rate_limits(
            IngestStorage::open_in_memory()
                .await
                .expect("storage should open"),
            2,
            Duration::from_secs(60 * 60),
            RATE_LIMIT_PER_SUBJECT_MAX_REQUESTS,
            RATE_LIMIT_PER_SUBJECT_WINDOW,
        );
        let router = build_router(state.clone());
        let addr = source_addr(6);

        for index in 0..2 {
            let payload = json!({
                "schema_version": 1,
                "telemetry_subject_id": format!("subject-ip-{index}"),
            });
            let response = router
                .clone()
                .oneshot(ingest_request(payload, addr))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-ip-another",
        });
        let response = router
            .oneshot(ingest_request(payload, addr))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// A country resolver that always returns a fixed code, so tests can exercise the country path
    /// without a GeoIP database.
    struct FixedCountryResolver(&'static str);
    impl country::CountryResolver for FixedCountryResolver {
        fn resolve(&self, _source_ip: std::net::IpAddr) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        serde_json::from_slice(&bytes).expect("body should be json")
    }

    #[tokio::test]
    async fn ingest_stores_server_derived_country_not_payload_country() {
        let state = StatsCollectorAppState::with_rate_limits(
            IngestStorage::open_in_memory()
                .await
                .expect("storage should open"),
            RATE_LIMIT_PER_IP_MAX_REQUESTS,
            RATE_LIMIT_PER_IP_WINDOW,
            RATE_LIMIT_PER_SUBJECT_MAX_REQUESTS,
            RATE_LIMIT_PER_SUBJECT_WINDOW,
        )
        .with_country_resolver(Arc::new(FixedCountryResolver("DE")));
        let router = build_router(state.clone());

        // Payload lies about being in the US; the server must record its own derived "DE".
        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-country",
            "country_code": "US",
        });
        let response = router
            .oneshot(ingest_request(payload, source_addr(20)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let records = state
            .storage
            .records_for_subject("subject-country")
            .await
            .unwrap();
        assert_eq!(records[0].country_code.as_deref(), Some("DE"));
    }

    #[tokio::test]
    async fn stats_summary_applies_k_anonymity_suppression() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        // 5 subjects on "common" (visible at k=5), 1 on "rare" (suppressed).
        for i in 0..5 {
            storage
                .insert(
                    i,
                    &format!("s-common-{i}"),
                    1,
                    None,
                    "{\"hardware_profile_id\":\"common\"}",
                )
                .await
                .unwrap();
        }
        storage
            .insert(100, "s-rare", 1, None, "{\"hardware_profile_id\":\"rare\"}")
            .await
            .unwrap();
        let state = StatsCollectorAppState::new(storage).with_k_anonymity_min(5);
        let router = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/stats/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["total_subjects"], 6);
        let profiles = body["by_hardware_profile"].as_array().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["hardware_profile_id"], "common");
    }

    #[tokio::test]
    async fn public_dashboard_exposes_only_k_anonymous_aggregate_data() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        for index in 0..5 {
            storage
                .insert(
                    index,
                    &format!("subject-common-{index}"),
                    1,
                    Some("CH"),
                    "{\"hardware_profile_id\":\"common\"}",
                )
                .await
                .unwrap();
        }
        storage
            .insert(
                100,
                "subject-rare",
                1,
                Some("LI"),
                "{\"hardware_profile_id\":\"rare\"}",
            )
            .await
            .unwrap();
        let router = build_router(StatsCollectorAppState::new(storage).with_k_anonymity_min(5));

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/stats/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "public, max-age=300, stale-while-revalidate=600"
        );
        let body = body_json(response).await;
        assert_eq!(body["schema_version"], 1);
        assert_eq!(body["software_version"], PACKAGE_VERSION);
        assert!(body["generated_at_unix"].as_i64().unwrap() > 0);
        assert_eq!(body["total_subjects"], 6);
        assert_eq!(body["by_hardware_profile"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["by_hardware_profile"][0]["hardware_profile_id"],
            "common"
        );
        assert_eq!(body["by_country"].as_array().unwrap().len(), 1);
        assert_eq!(body["by_country"][0]["country_code"], "CH");
        assert!(body.get("telemetry_subject_id").is_none());
        assert!(!body.to_string().contains("subject-common-0"));
        assert!(!body.to_string().contains("subject-rare"));
    }

    #[tokio::test]
    async fn admin_endpoints_return_412_when_no_token_configured() {
        let router = build_router(test_state().await);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/admin/raw?telemetry_subject_id=whatever")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn admin_raw_requires_matching_token() {
        let state = test_state()
            .await
            .with_admin_token(Some("secret".to_string()));
        let router = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/admin/raw?telemetry_subject_id=whatever")
                    .header(ADMIN_TOKEN_HEADER, "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_access_then_erasure_roundtrip() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage
            .insert(1, "subject-x", 1, None, "{\"hardware_profile_id\":\"p\"}")
            .await
            .unwrap();
        storage
            .insert(2, "subject-x", 1, None, "{\"hardware_profile_id\":\"p\"}")
            .await
            .unwrap();
        let state = StatsCollectorAppState::new(storage).with_admin_token(Some("tok".to_string()));
        let router = build_router(state.clone());

        // Access returns both records.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/admin/raw?telemetry_subject_id=subject-x")
                    .header(ADMIN_TOKEN_HEADER, "tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["records"].as_array().unwrap().len(), 2);

        // Erasure deletes them.
        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/admin/subject/subject-x")
                    .header(ADMIN_TOKEN_HEADER, "tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["deleted_records"], 2);
        assert_eq!(state.storage.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn registration_is_idempotent_and_returns_the_same_token() {
        let router = build_router(test_state().await);
        let addr = source_addr(30);

        let first = router
            .clone()
            .oneshot(register_request("subject-register", addr))
            .await
            .expect("router should respond");
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = body_json(first).await;
        let first_token = first_body["token"].as_str().unwrap().to_string();
        assert_eq!(first_token.len(), 64, "token should be 64 hex chars");

        // A second registration for the *same* subject id must return the same token, not mint a
        // new one - an operator re-running setup, or a node restarting before its first
        // successful ingest, must not silently invalidate its own credential.
        let second = router
            .oneshot(register_request("subject-register", addr))
            .await
            .expect("router should respond");
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = body_json(second).await;
        assert_eq!(second_body["token"], first_token);
    }

    #[tokio::test]
    async fn registration_gives_different_subjects_different_tokens() {
        let router = build_router(test_state().await);
        let addr = source_addr(31);

        let a = router
            .clone()
            .oneshot(register_request("subject-a", addr))
            .await
            .unwrap();
        let a_token = body_json(a).await["token"].as_str().unwrap().to_string();

        let b = router
            .oneshot(register_request("subject-b", addr))
            .await
            .unwrap();
        let b_token = body_json(b).await["token"].as_str().unwrap().to_string();

        assert_ne!(a_token, b_token);
    }

    #[tokio::test]
    async fn ingest_accepts_a_matching_ingestion_token() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let addr = source_addr(32);

        let register_response = router
            .clone()
            .oneshot(register_request("subject-token-match", addr))
            .await
            .unwrap();
        let token = body_json(register_response).await["token"]
            .as_str()
            .unwrap()
            .to_string();

        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-token-match",
        });
        let response = router
            .oneshot(ingest_request_with_token(payload, addr, Some(&token)))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            state
                .storage
                .records_for_subject("subject-token-match")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn ingest_rejects_a_mismatched_ingestion_token() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let addr = source_addr(33);

        router
            .clone()
            .oneshot(register_request("subject-token-mismatch", addr))
            .await
            .unwrap();

        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-token-mismatch",
        });
        let response = router
            .oneshot(ingest_request_with_token(
                payload,
                addr,
                Some("not-the-registered-token"),
            ))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            state
                .storage
                .records_for_subject("subject-token-mismatch")
                .await
                .unwrap()
                .len(),
            0,
            "a forged token must not result in a stored record"
        );
    }

    #[tokio::test]
    async fn ingest_rejects_a_token_when_none_is_registered_for_the_subject() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let addr = source_addr(34);

        // No registration call happened for this subject id at all, yet the request presents a
        // token - this can never match, so it is rejected the same way as an outright mismatch.
        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-never-registered",
        });
        let response = router
            .oneshot(ingest_request_with_token(
                payload,
                addr,
                Some("some-token-nobody-issued"),
            ))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            state
                .storage
                .records_for_subject("subject-never-registered")
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn ingest_still_accepts_a_request_with_no_token_at_all() {
        let state = test_state().await;
        let router = build_router(state.clone());
        let addr = source_addr(35);

        // Register a token for this subject id, but then send the ingest request without
        // presenting any token at all - doc Section 7/8's tolerance-first policy: older/
        // not-yet-upgraded nodes have nothing to present yet, so this must still be accepted.
        router
            .clone()
            .oneshot(register_request("subject-tokenless-ok", addr))
            .await
            .unwrap();

        let payload = json!({
            "schema_version": 1,
            "telemetry_subject_id": "subject-tokenless-ok",
        });
        let response = router
            .oneshot(ingest_request(payload, addr))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            state
                .storage
                .records_for_subject("subject-tokenless-ok")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn tokenless_ingestion_hits_a_stricter_per_subject_limit_than_tokened() {
        // A tight untokened limit (1/hour) so the test doesn't need to wait out a real window,
        // while the baseline subject_limiter stays generous enough not to interfere.
        let state = StatsCollectorAppState::with_rate_limits(
            IngestStorage::open_in_memory()
                .await
                .expect("storage should open"),
            RATE_LIMIT_PER_IP_MAX_REQUESTS,
            RATE_LIMIT_PER_IP_WINDOW,
            10,
            Duration::from_secs(60 * 60),
        )
        .with_untokened_subject_rate_limit(1, Duration::from_secs(60 * 60));
        let router = build_router(state.clone());
        let addr = source_addr(36);

        let payload = || {
            json!({
                "schema_version": 1,
                "telemetry_subject_id": "subject-tokenless-limited",
            })
        };

        let first = router
            .clone()
            .oneshot(ingest_request(payload(), addr))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);

        // The baseline subject_limiter (10/hour) would still allow this, but the dedicated,
        // stricter untokened-per-subject limiter (1/hour) must now kick in.
        let second = router
            .oneshot(ingest_request(payload(), addr))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn registration_endpoint_is_rate_limited_per_ip() {
        let state = test_state()
            .await
            .with_register_rate_limit(2, Duration::from_secs(60 * 60));
        let router = build_router(state);
        let addr = source_addr(37);

        for index in 0..2 {
            let response = router
                .clone()
                .oneshot(register_request(&format!("subject-rl-{index}"), addr))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        // A third registration attempt from the same IP, even for yet another subject id, must be
        // blocked by the dedicated registration rate limiter.
        let response = router
            .oneshot(register_request("subject-rl-another", addr))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn registration_rejects_an_empty_subject_id() {
        let router = build_router(test_state().await);
        let addr = source_addr(38);

        // A path segment of only whitespace, percent-encoded, trims down to empty.
        let request = Request::builder()
            .method("POST")
            .uri("/v1/register/%20%20")
            .extension(ConnectInfo(addr))
            .body(Body::empty())
            .expect("request should build");
        let response = router
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
