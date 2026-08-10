use anyhow::Context;
use log::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

mod connection;
use connection::Connection;

mod database;
use database::Database;

pub mod charging_session;
pub use charging_session::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};
pub mod measurements;
pub mod schedule;

const PORT: u16 = 9000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .format_source_path(true)
        .format_line_number(true)
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .try_init()
        .unwrap();

    // make sure the DB is available
    let _ = Database::get();

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bindind to {addr}"))?;
    info!("Listening on: {addr}");

    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr().context("getting peer address")?;
        info!("peer address {peer}");

        let mut connection = Connection::new(peer, accept_async(stream).await.expect("can accept"));
        if let Err(err) = connection.run_loop().await {
            error!("{}: {err:#}", connection.peer());
        }
    }

    Ok(())
}
