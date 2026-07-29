use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use patchbay::Device;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};

/// Control endpoint used by clients to request an Iroh relay ticket.
pub const IROH_RELAY_TICKET_PATH: &str = "/control/iroh-relay/ticket";

#[derive(Clone)]
struct TicketTimeoutState {
    shutdown_tx: watch::Sender<bool>,
}

impl TicketTimeoutState {
    fn new() -> Self {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        Self { shutdown_tx }
    }
}

fn ticket_timeout_router_with_state(state: TicketTimeoutState) -> Router {
    Router::new()
        .fallback(ticket_timeout_handler)
        .with_state(state)
}

async fn ticket_timeout_handler(
    State(state): State<TicketTimeoutState>,
    request: Request<Body>,
) -> Response {
    if request.method() == Method::POST && request.uri().path() == IROH_RELAY_TICKET_PATH {
        let mut shutdown_rx = state.shutdown_tx.subscribe();
        if !*shutdown_rx.borrow() {
            let _ = shutdown_rx.wait_for(|shutdown| *shutdown).await;
        }
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "intentional rendezvous fault",
        )
            .into_response()
    }
}

/// Iroh relay-ticket fault server running inside a Patchbay device namespace.
///
/// Call [`shutdown`](Self::shutdown) to wait for a graceful server exit. If
/// the handle is dropped, `Drop` still signals shutdown and aborts the task as
/// a final safeguard, so the listener cannot outlive its owner.
pub struct TicketTimeoutServer {
    local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl TicketTimeoutServer {
    /// Binds and serves the fault injector from `device`'s network namespace.
    ///
    /// Port `0` is supported; use [`url`](Self::url) to discover the assigned
    /// address.
    pub async fn spawn(device: &Device, bind_addr: SocketAddr) -> Result<Self> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let fault_state = TicketTimeoutState::new();
        let fault_shutdown_tx = fault_state.shutdown_tx.clone();
        let task = device
            .spawn(move |_device| async move {
                let listener = match TcpListener::bind(bind_addr).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        let message =
                            format!("failed binding ticket-timeout server at {bind_addr}: {error}");
                        let _ = ready_tx.send(Err(message.clone()));
                        bail!(message);
                    }
                };
                let local_addr = match listener.local_addr() {
                    Ok(local_addr) => local_addr,
                    Err(error) => {
                        let message =
                            format!("failed reading ticket-timeout server address: {error}");
                        let _ = ready_tx.send(Err(message.clone()));
                        bail!(message);
                    }
                };
                if ready_tx.send(Ok(local_addr)).is_err() {
                    bail!("ticket-timeout server owner disappeared during startup");
                }

                axum::serve(listener, ticket_timeout_router_with_state(fault_state))
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                        fault_shutdown_tx.send_replace(true);
                    })
                    .await
                    .context("ticket-timeout server exited with an error")
            })
            .context("failed spawning ticket-timeout server in Patchbay device")?;

        let listener_addr = match ready_rx.await {
            Ok(Ok(local_addr)) => local_addr,
            Ok(Err(message)) => {
                let _ = task
                    .await
                    .context("ticket-timeout server task panicked after bind failure")?
                    .ok();
                bail!(message);
            }
            Err(_) => {
                let result = task
                    .await
                    .context("ticket-timeout server task panicked during startup")?;
                return Err(match result {
                    Ok(()) => anyhow!("ticket-timeout server exited before reporting readiness"),
                    Err(error) => {
                        error.context("ticket-timeout server failed before reporting readiness")
                    }
                });
            }
        };
        let local_addr = match reachable_addr(device, listener_addr) {
            Ok(local_addr) => local_addr,
            Err(error) => {
                let _ = shutdown_tx.send(());
                task.await
                    .context("ticket-timeout server task panicked during failed startup")??;
                return Err(error);
            }
        };

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.await
                .context("ticket-timeout server task panicked during shutdown")??;
        }
        Ok(())
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

fn reachable_addr(device: &Device, bind_addr: SocketAddr) -> Result<SocketAddr> {
    let ip = match bind_addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            IpAddr::V4(device.ip().context("Patchbay device has no IPv4 address")?)
        }
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(
            device
                .ip6()
                .context("Patchbay device has no IPv6 address")?,
        ),
        ip => ip,
    };
    Ok(SocketAddr::new(ip, bind_addr.port()))
}

impl Drop for TicketTimeoutServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn ticket_post_stalls() {
        let state = TicketTimeoutState::new();
        let request = Request::builder()
            .method(Method::POST)
            .uri(IROH_RELAY_TICKET_PATH)
            .body(Body::empty())
            .unwrap();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                ticket_timeout_handler(State(state), request),
            )
            .await
            .is_err(),
            "ticket request unexpectedly completed"
        );
    }

    #[tokio::test]
    async fn shutdown_releases_stalled_ticket_post() {
        let state = TicketTimeoutState::new();
        state.shutdown_tx.send_replace(true);
        let request = Request::builder()
            .method(Method::POST)
            .uri(IROH_RELAY_TICKET_PATH)
            .body(Body::empty())
            .unwrap();

        let response = tokio::time::timeout(
            Duration::from_millis(100),
            ticket_timeout_handler(State(state), request),
        )
        .await
        .expect("shutdown should release a stalled ticket request");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn all_other_requests_fail_immediately() {
        let state = TicketTimeoutState::new();
        for (method, path) in [
            (Method::GET, IROH_RELAY_TICKET_PATH),
            (Method::POST, "/control/relay/ticket"),
            (Method::GET, "/health"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let response = tokio::time::timeout(
                Duration::from_millis(100),
                ticket_timeout_handler(State(state.clone()), request),
            )
            .await
            .expect("non-ticket request should complete immediately");
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }
}
