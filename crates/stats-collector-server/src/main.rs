//! Binary entry point for the hardware-reliability stats collector service.
//!
//! Configuration is via environment variables only (no domain/TLS is hardcoded here — where and
//! how this binds is a deployment concern, see
//! `docs/server-node-hardware-reliability-telemetry-strategy.md` Section 5.2):
//! - `STATS_COLLECTOR_BIND_ADDR`: address to bind, defaults to `127.0.0.1:44044`.
//! - `STATS_COLLECTOR_DB_PATH`: path to the Turso (embedded, SQLite-compatible) database file,
//!   defaults to `stats-collector.sqlite3` in the current working directory.
//! - `STATS_COLLECTOR_ADMIN_TOKEN`: bearer token guarding the raw-access / erasure endpoints
//!   (doc Sections 4.5, 5.3). When unset, those endpoints return 412 instead of operating
//!   unauthenticated.
//! - `STATS_COLLECTOR_K_ANONYMITY_MIN`: minimum group size for published aggregates
//!   (doc Section 4.3), defaults to 5.
//! - `STATS_COLLECTOR_RETENTION_DAYS`: raw-data retention window before pruning (doc Section 4.6),
//!   defaults to 180.
//! - `STATS_COLLECTOR_TLS_CERT_PATH` and `STATS_COLLECTOR_TLS_KEY_PATH`: optional PEM files for
//!   serving HTTPS directly. Both must be set together. The files are reloaded periodically so a
//!   hosting provider can renew them in place without requiring a redeployment.
//! - `STATS_COLLECTOR_TLS_RELOAD_SECS`: TLS file reload interval, defaults to 24 hours.
//!
//! The public fleet dashboard is compiled into this binary. It is served from the same HTTPS
//! origin as `/v1/stats/dashboard`, so browser requests need no cross-origin privileges or a
//! separate static-file deployment.

use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(feature = "bundled-country-db")]
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::OriginalUri;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use percent_encoding::percent_decode_str;
#[cfg(feature = "bundled-country-db")]
use stats_collector_server::country::BundledCountryResolver;
use stats_collector_server::{
    DEFAULT_BIND_ADDR, DEFAULT_DB_PATH, DEFAULT_K_ANONYMITY_MIN, DEFAULT_RETENTION_DAYS,
    StatsCollectorAppState, build_router, storage::IngestStorage,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod embedded_dashboard {
    use super::*;

    mod generated_assets {
        include!(concat!(env!("OUT_DIR"), "/fleet_telemetry_assets.rs"));
    }

    pub async fn serve(OriginalUri(uri): OriginalUri) -> Response {
        let request_path = uri.path().strip_prefix('/').unwrap_or(uri.path());
        let asset_path = if request_path.is_empty() {
            "index.html".into()
        } else {
            match percent_decode_str(request_path).decode_utf8() {
                Ok(path) => path,
                Err(_) => return StatusCode::NOT_FOUND.into_response(),
            }
        };

        match generated_assets::asset(&asset_path) {
            Some((bytes, content_type, cache_control)) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache_control),
                ],
                bytes,
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// How often to sweep stale per-key rate-limit bookkeeping (see
/// `StatsCollectorAppState::cleanup_rate_limiters`).
const RATE_LIMIT_CLEANUP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often to enforce raw-data retention (doc Section 4.6). Daily is ample for a day-granularity
/// cutoff.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_TLS_RELOAD_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone)]
struct TlsFiles {
    cert_path: PathBuf,
    key_path: PathBuf,
    reload_interval: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| matches!(arg.as_str(), "--version" | "-V"))
    {
        println!("stats-collector-server {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    init_tracing();

    let bind_addr: SocketAddr = std::env::var("STATS_COLLECTOR_BIND_ADDR")
        .unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string())
        .parse()
        .context("invalid STATS_COLLECTOR_BIND_ADDR")?;
    let db_path =
        std::env::var("STATS_COLLECTOR_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());
    let admin_token = std::env::var("STATS_COLLECTOR_ADMIN_TOKEN").ok();
    let k_anonymity_min = std::env::var("STATS_COLLECTOR_K_ANONYMITY_MIN")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_K_ANONYMITY_MIN);
    let retention_days = std::env::var("STATS_COLLECTOR_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    let tls_files = tls_files_from_env()?;

    if admin_token.is_none() {
        warn!(
            "STATS_COLLECTOR_ADMIN_TOKEN is not set; the admin raw-access and erasure endpoints will return 412"
        );
    }

    let storage = IngestStorage::open(&db_path)
        .await
        .with_context(|| format!("failed to open stats-collector database at {db_path}"))?;
    let state = StatsCollectorAppState::new(storage)
        .with_admin_token(admin_token)
        .with_k_anonymity_min(k_anonymity_min);
    #[cfg(feature = "bundled-country-db")]
    let state = {
        info!("using bundled offline IP-to-country resolver");
        state.with_country_resolver(Arc::new(BundledCountryResolver))
    };

    spawn_rate_limit_cleanup(state.clone());
    spawn_retention_sweeper(state.clone(), retention_days);

    let app = build_app(state);

    info!(
        %bind_addr,
        db_path = %db_path,
        k_anonymity_min,
        retention_days,
        tls = tls_files.is_some(),
        public_dashboard = true,
        "stats-collector-server listening"
    );

    match tls_files {
        Some(files) => {
            let tls_config = RustlsConfig::from_pem_file(&files.cert_path, &files.key_path)
                .await
                .with_context(|| {
                    format!(
                        "failed loading TLS certificate {} and key {}",
                        files.cert_path.display(),
                        files.key_path.display()
                    )
                })?;
            spawn_tls_reloader(tls_config.clone(), files);
            axum_server::bind_rustls(bind_addr, tls_config)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .context("TLS stats-collector-server stopped unexpectedly")?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .with_context(|| format!("failed to bind {bind_addr}"))?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .context("stats-collector-server stopped unexpectedly")?;
        }
    }

    Ok(())
}

fn build_app(state: StatsCollectorAppState) -> Router {
    build_router(state).fallback(get(embedded_dashboard::serve))
}

fn tls_files_from_env() -> Result<Option<TlsFiles>> {
    let cert_path = std::env::var_os("STATS_COLLECTOR_TLS_CERT_PATH").map(PathBuf::from);
    let key_path = std::env::var_os("STATS_COLLECTOR_TLS_KEY_PATH").map(PathBuf::from);
    match (cert_path, key_path) {
        (None, None) => Ok(None),
        (Some(cert_path), Some(key_path)) => {
            let reload_secs = std::env::var("STATS_COLLECTOR_TLS_RELOAD_SECS")
                .ok()
                .map(|value| {
                    value
                        .parse::<u64>()
                        .context("invalid STATS_COLLECTOR_TLS_RELOAD_SECS")
                })
                .transpose()?
                .unwrap_or(DEFAULT_TLS_RELOAD_SECS);
            anyhow::ensure!(
                reload_secs > 0,
                "STATS_COLLECTOR_TLS_RELOAD_SECS must be greater than zero"
            );
            Ok(Some(TlsFiles {
                cert_path,
                key_path,
                reload_interval: Duration::from_secs(reload_secs),
            }))
        }
        _ => anyhow::bail!(
            "STATS_COLLECTOR_TLS_CERT_PATH and STATS_COLLECTOR_TLS_KEY_PATH must be set together"
        ),
    }
}

fn spawn_tls_reloader(config: RustlsConfig, files: TlsFiles) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(files.reload_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
            match config
                .reload_from_pem_file(&files.cert_path, &files.key_path)
                .await
            {
                Ok(()) => info!(
                    cert_path = %files.cert_path.display(),
                    "reloaded stats-collector TLS certificate"
                ),
                Err(error) => warn!(
                    %error,
                    cert_path = %files.cert_path.display(),
                    "failed reloading stats-collector TLS certificate; keeping the previous configuration"
                ),
            }
        }
    });
}

fn spawn_rate_limit_cleanup(state: StatsCollectorAppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RATE_LIMIT_CLEANUP_INTERVAL);
        loop {
            interval.tick().await;
            state.cleanup_rate_limiters();
        }
    });
}

fn spawn_retention_sweeper(state: StatsCollectorAppState, retention_days: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            match state.prune_expired(retention_days).await {
                Ok(removed) if removed > 0 => {
                    info!(removed, retention_days, "pruned expired telemetry records")
                }
                Ok(_) => {}
                Err(error) => warn!(%error, "failed to prune expired telemetry records"),
            }
        }
    });
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use stats_collector_server::storage::IngestStorage;
    use tower::ServiceExt;

    #[tokio::test]
    async fn compiled_in_dashboard_serves_the_index_and_hashed_assets() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        let app = build_app(StatsCollectorAppState::new(storage));

        let index_response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("dashboard should respond");
        assert_eq!(index_response.status(), StatusCode::OK);
        assert_eq!(index_response.headers()[header::CACHE_CONTROL], "no-cache");
        let index_body = axum::body::to_bytes(index_response.into_body(), usize::MAX)
            .await
            .expect("dashboard index should be readable");
        let index_html = String::from_utf8(index_body.to_vec()).expect("dashboard index is UTF-8");
        assert!(index_html.contains("<title>IronMesh Fleet Reliability</title>"));
        let script_path = index_html
            .split("<script")
            .find_map(|tag| tag.split("src=\"").nth(1))
            .and_then(|value| value.split('"').next())
            .expect("dashboard index should reference a script");
        assert!(script_path.starts_with("/assets/"));
        assert!(script_path.ends_with(".js"));

        let asset_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(script_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("embedded dashboard asset should respond");
        assert_eq!(asset_response.status(), StatusCode::OK);
        assert_eq!(
            asset_response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            asset_response.headers()[header::CONTENT_TYPE],
            "application/javascript; charset=utf-8"
        );

        let favicon_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ironmesh-favicon.svg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("compiled-in public asset should respond");
        assert_eq!(favicon_response.status(), StatusCode::OK);
        assert_eq!(
            favicon_response.headers()[header::CONTENT_TYPE],
            "image/svg+xml"
        );

        let percent_encoded_favicon_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/%69ronmesh-favicon.svg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("percent-encoded compiled-in public asset should respond");
        assert_eq!(percent_encoded_favicon_response.status(), StatusCode::OK);

        let unknown_path_response = app
            .oneshot(Request::builder().uri("/v1").body(Body::empty()).unwrap())
            .await
            .expect("unknown API-adjacent path should respond");
        assert_eq!(unknown_path_response.status(), StatusCode::NOT_FOUND);
    }
}
