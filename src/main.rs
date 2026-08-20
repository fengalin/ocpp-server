use anyhow::{Context, bail};
use clap::Parser;
use futures::{pin_mut, prelude::*};
use log::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

mod args;
use args::Args;

mod bms;
use bms::{Bms, SocProgress};

mod connection;
use connection::Connection;

mod database;
use database::Database;

pub mod charging_session;
pub use charging_session::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};
pub mod measurements;
pub mod schedule;
pub use schedule::{ChargingPlan, ChargingSchedule, ChargingSchedulePeriod, ChargingScheduleState};

#[cfg(test)]
pub mod tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    env_logger::builder()
        .format_source_path(true)
        .format_line_number(true)
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .try_init()
        .unwrap();

    let bms = Bms::from(&args);
    info!("Specs: {bms:#?}");

    let charging_plan = Option::<ChargingPlan>::try_from(&args)
        .inspect(|cp| {
            if args.is_dry_run() {
                info!("Charging plan: {cp:?}");
                if let Some(cp) = cp {
                    let _ = cp.to_charging_schedule(&bms);
                }
            }
        })
        .inspect_err(|err| error!("Charging plan: {err}"))?;

    // make sure the DB is available
    {
        let db = Database::get();

        match db.get_last_charging_schedule() {
            Ok(Some(schedule)) => {
                info!("last known schedule:\n\t{schedule}");
            }
            Ok(None) => {
                info!("no known charging schedule");
            }
            Err(err) => {
                bail!("failed to query last charging schedule: {err}");
            }
        }

        match db.get_last_charging_session(&bms) {
            Ok(Some(sess)) => {
                info!(
                    "last known session:\n\
                        \tid: {}, state: {}, soc cap: {:?}, last soc: {:?}
                        ",
                    sess.session_id(),
                    sess.state(),
                    sess.bms().soc_cap,
                    sess.last_soc()
                );
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
    }

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

    let ctrl_c = tokio::signal::ctrl_c().fuse();
    pin_mut!(ctrl_c);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bindind to {addr}"))?;
    info!("Listening on: {addr}");

    if args.is_dry_run() {
        warn!("Quitting now!\nUse the 'run' command to actually run the server");
        return Ok(());
    }

    if args.is_reset() {
        warn!("Resetting... this will cancel any charging plan (limit will be set to 0 W)");
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
                    db.get_last_charging_schedule()
                        .context("getting last charging schedule")?,
                )
            };

            let mut connection = Connection::new(
                ws_stream,
                bms.clone(),
                charging_plan,
                last_charging_session,
                last_charging_schedule,
                args.command.expect("not dry-run"),
            );

            if let Err(err) = connection.run_loop(ctrl_c.as_mut()).await {
                error!("{peer}: {err:#}");
            }
        }
        complete => (),
    }

    Ok(())
}
