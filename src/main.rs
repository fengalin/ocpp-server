use anyhow::{Context, bail};
use clap::Parser;
use futures::{pin_mut, prelude::*};
use log::*;
#[cfg(feature = "systemd")]
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

mod args;
use args::{Args, Command};

mod bms;
use bms::{Bms, SoC, SoCProgress};
pub mod charging_session;
pub use charging_session::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};
mod connection;
use connection::Connection;
mod database;
use database::Database;
mod evse;
use evse::Evse;
pub mod measurements;
mod ocpp;
use ocpp::{CommandToChargingPoint, OcppInterface};
pub mod schedule;
pub use schedule::{ChargingPlan, ChargingSchedule, ChargingSchedulePeriod, ChargingScheduleState};

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

    #[cfg(feature = "systemd")]
    env_logger_builder
        // when used as a systemd unit, timestamps are already logged
        .format_timestamp(None)
        // only keep the log level, not the crate/module
        .format(|buf, record| writeln!(buf, "{}  {}", record.level(), record.args()));

    env_logger_builder
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .try_init()
        .unwrap();

    let mut bms = Bms::from(&args);
    info!("User defined BMS:\n{bms}");

    {
        let db = Database::get();

        match db.check_last_charging_session(&bms) {
            Ok(Some(session)) => {
                info!("last known session:\n{session}");
                if !session.is_complete() {
                    bms = session.bms().clone();
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
                            "\noutstanding: {}",
                            schedule.outstanding(
                                chrono::Local::now().naive_local(),
                                bms.constant_power_loss
                            )
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
                if let Some(s) = cp.to_charging_schedule(&bms) {
                    let outstanding =
                        s.outstanding(chrono::Local::now().naive_local(), bms.constant_power_loss);
                    info!(
                        "outstanding: {outstanding}{}",
                        if !outstanding.is_zero() {
                            format!(", {}", SoC::Relative(outstanding.energy / bms.capacity))
                        } else {
                            "".to_string()
                        }
                    );
                }
            }
        })
        .inspect_err(|err| error!("Charging plan: {err}"))?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

    let ctrl_c = tokio::signal::ctrl_c().fuse();
    pin_mut!(ctrl_c);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bindind to {addr}"))?;
    info!("Listening on: {addr}");

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

    let accept_stream = listener.accept().fuse();
    pin_mut!(accept_stream);

    futures::select_biased! {
        _ = ctrl_c => {
            warn!("shutting down due to SIGINT");
        }
        accept_res = accept_stream => {
            let Ok((stream, _)) = accept_res else {
                bail!("TCP listener terminated");
            };
            let peer = stream.peer_addr().context("getting peer address")?;
            let ws_stream = accept_async(stream).await.context("accepting ws stream")?;

            info!("peer address {peer}");

            let (last_charging_session, last_charging_schedule) = {
                let db = Database::get();
                (
                    db.get_last_charging_session(&bms)
                        .context("getting last charging session")?,
                    db.get_active_charging_schedule()
                        .context("getting last charging schedule")?,
                )
            };

            let evse = Evse::new(
                bms.clone(),
                last_charging_session,
                last_charging_schedule,
            );

            let mut connection = Connection::new(
                ws_stream,
                evse,
                args.command.expect("not dry-run"),
                charging_plan,
            );

            if let Err(err) = connection.run_loop(ctrl_c.as_mut()).await {
                error!("{peer}: {err:#}");
            }
        }
        complete => (),
    }

    Ok(())
}
