use anyhow::{Context, bail};
use clap::Parser;
use futures::{pin_mut, prelude::*};
use log::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

mod args;
use args::{Args, Bms, ChargingPlan};

mod connection;
use connection::Connection;

mod database;
use database::Database;

pub mod charging_session;
pub use charging_session::{
    ChargingSession, ChargingSessionSnapshot, ChargingSessionState, SocProgress,
};
pub mod measurements;
pub mod schedule;

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

    let battery = Bms::from(&args);
    info!("Specs: {battery:#?}");
    info!("Charging plan: {:?}", args.charging_plan);

    // make sure the DB is available
    let _ = Database::get();

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

    let ctrl_c = tokio::signal::ctrl_c().fuse();
    pin_mut!(ctrl_c);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bindind to {addr}"))?;
    let accept_stream = listener.accept().fuse();
    pin_mut!(accept_stream);

    info!("Listening on: {addr}");

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

                let active_charging_session = Database::get().get_last_active_charging_session()
                    .context("getting last active charging session")?;

                info!("peer address {peer}");

                let mut connection = Connection::new(peer, ws_stream, battery,
                    args.charging_plan, active_charging_session);
                if let Err(err) = connection.run_loop(ctrl_c.as_mut()).await {
                    error!("{}: {err:#}", connection.peer());
                }
            }
            complete => break,
        }
    }

    Ok(())
}
