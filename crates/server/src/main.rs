use anyhow::{Context, bail};
use clap::Parser;
use log::*;
use tokio::sync::{broadcast, mpsc};

use std::io::Write;

use charge_system_common_model::{
    bms::{self, Bms, SoC},
    plan_model::ChargingPlan,
};

mod args;
use args::{Args, Command};

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
use ocpp::{CommandToChargingPoint, OcppInterface};
pub mod schedule;
pub use schedule::{ChargingSchedule, ChargingSchedulePeriod, ChargingScheduleState};
pub mod unix_socket;
pub use unix_socket::UnixSocketNotifier;
pub mod webserver;
pub use webserver::Webserver;

#[cfg(test)]
pub mod tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.check()?;

    let mut env_logger_builder = env_logger::builder();

    #[cfg(debug_assertions)]
    env_logger_builder
        .format_source_path(true)
        .format_line_number(true);

    if args.short_logs {
        env_logger_builder
            // when used as a systemd unit, timestamps are already logged
            .format_timestamp(None)
            // only keep the log level, not the crate/module
            .format(|buf, record| writeln!(buf, "{}  {}", record.level(), record.args()));
    }

    env_logger_builder
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .try_init()
        .unwrap();

    let mut bms = Bms::from(&args);
    info!("User defined BMS:\n{bms}");

    {
        let db = Database::get();

        match db.check_last_charging_session(&mut bms) {
            Ok(Some(session)) => {
                info!("last known session:\n{session}");
                if !session.is_complete() {
                    info!("consolidated BMS:\n{bms}");
                }
            }
            Ok(None) => {
                if args.is_stop_session() {
                    bail!("no known charging session to stop");
                } else {
                    info!("no known charging session");
                }
            }
            Err(err) => {
                bail!("failed to query last session: {err}");
            }
        }

        match db.get_active_charging_schedule() {
            Ok(Some(schedule)) => {
                info!(
                    "active charging schedule:\n{schedule}{}",
                    if !schedule.is_empty() {
                        format!(
                            "\nremaining: {}",
                            schedule
                                .remaining(chrono::Local::now().naive_local(), bms::STATIC_LOSS)
                        )
                    } else {
                        "".to_string()
                    }
                );
            }
            Ok(None) => {
                info!("no active charging schedule");
            }
            Err(err) => {
                bail!("failed to query active charging schedule: {err}");
            }
        }
    }

    let charging_plan = Option::<ChargingPlan>::try_from(&args)
        .inspect(|cp| {
            if args.is_dry_run()
                && let Some(cp) = cp
            {
                info!("Selected charging plan: {cp:?}");
                match ChargingSchedule::try_from_charging_plan(cp, &bms) {
                    Ok(Some(s)) => {
                        let remaining =
                            s.remaining(chrono::Local::now().naive_local(), bms::STATIC_LOSS);
                        info!(
                            "remaining: {remaining}{}",
                            if !remaining.is_zero() {
                                format!(
                                    ", {}",
                                    SoC::Relative(remaining.energy as f32 / bms.capacity as f32)
                                )
                            } else {
                                "".to_string()
                            }
                        );
                    }
                    Ok(None) => warn!("unlimited charging plan"),
                    Err(err) => error!("couldn't compute charging schedule: {err}"),
                }
            }
        })
        .inspect_err(|err| error!("Charging plan: {err}"))?;

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
    let webserver = Webserver::new(&args, notif_tx.clone(), cmd_tx).await?;
    let dispatcher = Dispatcher::new(bms, &args, notif_tx, cmd_rx, charging_plan).await?;

    match &args.command {
        None => {
            warn!("Quitting now!\nUse the 'run' command to actually run the server");
            return Ok(());
        }
        Some(Command::Run) => {
            info!("starting the server");
        }
        Some(Command::Reboot) => {
            warn!("/!\\ Rebooting");
            warn!("any charging plans will be replaced by a permanent 0 W plan");
        }
        Some(Command::SetServerIp(ip_address)) => {
            let server_ip = ip_address
                .get_ip_address()
                .context("set-server-ip command")?;
            warn!("Setting server IP address: {server_ip}");
            warn!("/!\\ the charging point will Reboot");
            warn!("any charging plans will be replaced by a permanent 0 W plan");
        }
        _ => (),
    }

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
