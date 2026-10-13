/// Waits for the process to receive either Ctrl-C or an operating-system
/// termination request.
///
/// On Unix, termination requests are delivered through `SIGTERM`. On
/// platforms without a separate termination signal, Ctrl-C remains the only
/// trigger. A signal-registration failure is logged and leaves the other
/// signal source active.
pub async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed waiting for ctrl-c signal");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                let _ = signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "failed installing SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
