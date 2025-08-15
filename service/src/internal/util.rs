use log::info;
use serde::{Deserialize, Serialize};
use tokio::signal::{
    self,
    unix::{signal as unix_signal, SignalKind},
};

/// Account for shutdown signals, `ctrl+c` and other common Unix signals.
#[cfg(target_os = "linux")]
pub async fn wait_shutdown_signals() {
    // Wait for the Ctrl+C signal.
    let ctrl_c = signal::ctrl_c();

    // Create listeners for SIGTERM, SIGINT, and SIGHUP.
    let mut sigterm =
        unix_signal(SignalKind::terminate()).expect("Failed to set up SIGTERM listener");
    let mut sigint =
        unix_signal(SignalKind::interrupt()).expect("Failed to set up SIGINT listener");
    let mut sighup = unix_signal(SignalKind::hangup()).expect("Failed to set up SIGHUP listener");

    info!("Listening for shutdown signals (ctrl+c, SIGTERM, SIGINT, SIGHUP)");

    // Wait for any of the signals to occur.
    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C.");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM.");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT.");
        }
        _ = sighup.recv() => {
            info!("Received SIGHUP.");
        }
    }
}

/// Account for shutdown signals, `ctrl+c`
///
/// TODO: handle OS specific signals
#[cfg(not(target_os = "linux"))]
pub async fn wait_shutdown_signals() {
    // Wait for the Ctrl+C signal.
    let ctrl_c = signal::ctrl_c();

    info!("Listening for shutdown signals (ctrl+c)");

    // Wait for any of the signals to occur.
    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C.");
        }
    }
}
