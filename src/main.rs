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
pub use schedule::ChargingPlan;

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
            if !args.run {
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
        match db.get_last_active_charging_session(None) {
            Ok(Some(sess)) => {
                info!(
                    "found active session:\n\
                        \tid: {}, soc cap: {:?}, last soc: {:?}
                        ",
                    sess.session_id(),
                    sess.bms().soc_cap,
                    sess.last_soc()
                );
            }
            Ok(None) => info!("no active charging session"),
            Err(err) => {
                error!("failed to query last active session: {err}");
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

    if !args.run {
        warn!("Quitting now!\nUse --run to actually run the server");
        return Ok(());
    }

    let accept_stream = listener.accept().fuse();
    pin_mut!(accept_stream);

    loop {
        futures::select_biased! {
            _ = ctrl_c => {
                warn!("shutting down due to SIGINT");
                break;
            }
            accept_res = accept_stream => {
                let Ok((stream, _)) = accept_res else {
                    bail!("TCP listener terminated");
                };
                let peer = stream.peer_addr().context("getting peer address")?;
                let ws_stream = accept_async(stream).await.context("accepting ws stream")?;

                let active_charging_session = Database::get().get_last_active_charging_session(&bms)
                    .context("getting last active charging session")?;

                info!("peer address {peer}");

                let mut connection = Connection::new(peer, ws_stream, bms,
                    charging_plan, active_charging_session);
                if let Err(err) = connection.run_loop(ctrl_c.as_mut()).await {
                    // FIXME when connection is lost due to a reboot,
                    // (Connection reset without closing handshake)
                    // the charging point doesn't seem to be able to connect again
                    error!("{}: {err:#}", connection.peer());
                }
            }
            complete => break,
        }
    }

    Ok(())
}
