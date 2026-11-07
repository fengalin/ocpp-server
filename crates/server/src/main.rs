use clap::Parser;
use log::*;
use tokio::sync::{broadcast, mpsc};

use charge_system_common_model::bms::Bms;

mod args;
use args::Args;

pub mod charging_session;
pub use charging_session::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};
mod dispatcher;
use dispatcher::Dispatcher;
mod database;
use database::Database;
mod evse;
use evse::Evse;
pub mod measurements;
mod ocpp;
use ocpp::{CommandSender, CommandToChargingPoint, OcppInterface};
pub mod schedule;
pub use schedule::{ChargingSchedule, ChargingScheduleState};
pub mod unix_socket;
pub use unix_socket::UnixSocketNotifier;
pub mod webserver;
pub use webserver::Webserver;

#[cfg(test)]
pub mod tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::{filter::LevelFilter, prelude::*};

    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    let with_debug = env_filter
        .max_level_hint()
        .is_some_and(|l| l >= LevelFilter::DEBUG);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_thread_ids(with_debug)
                .with_line_number(with_debug),
        )
        .with(env_filter)
        .init();

    let args = Args::parse();
    let bms = Bms::from(&args);

    // The shared notification channel fans out to the unix-socket notifier,
    // the web server (recording task + one SSE subscriber per connection) and
    // dispatcher/evse senders; each subscriber gets its own 64-slot window,
    // a lagging consumer just resyncs from the REST snapshot.
    let (notif_tx, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel(8);

    // Both subscriptions below must be created BEFORE Dispatcher::new: Evse::new
    // broadcasts a startup BmsUpdate during construction and a broadcast only
    // buffers for receivers that already exist.
    let mut unix_socket_notif = UnixSocketNotifier::new(notif_tx.subscribe())
        .inspect_err(|err| error!("failed to create unix socket notifier: {err}"))
        .ok();
    let webserver = Webserver::new(&args, notif_tx.clone(), CommandSender::new(cmd_tx)).await?;
    let dispatcher = Dispatcher::new(bms, &args, notif_tx, cmd_rx).await?;

    info!("starting the server");

    let (stop_tx, stop_rx) = broadcast::channel(8);

    let unix_socket_notif_hdl = unix_socket_notif
        .take()
        .map(|unix_socket_notif| tokio::spawn(unix_socket_notif.into_task(stop_rx)));
    let webserver_hdl = tokio::spawn(webserver.into_task(stop_tx.subscribe()));
    let dispatcher_hdl = tokio::spawn(dispatcher.into_task(stop_tx.subscribe()));

    let _ = tokio::signal::ctrl_c().await;
    warn!("shutting down due to SIGINT");
    let _ = stop_tx.send(());

    if let Some(unix_socket_notif_hdl) = unix_socket_notif_hdl {
        let _ = unix_socket_notif_hdl.await;
    }
    let _ = webserver_hdl.await;
    let _ = dispatcher_hdl.await;

    Ok(())
}
