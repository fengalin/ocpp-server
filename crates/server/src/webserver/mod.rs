use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::Context;
use log::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use charge_system_common_model::notification::Notification;

use crate::{CommandToChargingPoint, args};

mod api;
mod state;
mod world;
use state::AppState;

#[derive(Debug)]
pub struct Webserver {
    listener: TcpListener,
    notif_tx: broadcast::Sender<Notification>,
    notif_rx: broadcast::Receiver<Notification>,
    cmd_tx: mpsc::Sender<CommandToChargingPoint>,
}

impl Webserver {
    pub async fn new(
        args: &args::Args,
        // we need the sender to subscribe to it when
        // new client connect, without buffering messages unnecessary
        notif_tx: broadcast::Sender<Notification>,
        cmd_tx: mpsc::Sender<CommandToChargingPoint>,
    ) -> anyhow::Result<Self> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.webserver_port);

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding to {addr}"))?;
        info!("Listening on: {addr}");

        // subscribe right now so we don't miss the initial notifications
        // even if they are sent before we start polling
        let notif_rx = notif_tx.subscribe();

        Ok(Webserver {
            listener,
            notif_tx,
            notif_rx,
            cmd_tx,
        })
    }

    pub async fn into_task(self, mut stop_rx: broadcast::Receiver<()>) {
        let appstate = AppState::new(self.cmd_tx, self.notif_tx);

        // The recording task is the sole writer of `world`; SSE clients
        // subscribe to the channel on their own (fresh receiver per
        // connection).
        let recorder_hdl = tokio::spawn(appstate.clone().into_task(self.notif_rx));

        tokio::select! {
            biased;
            _  = stop_rx.recv() => {
                info!("shutting down due to stop request");
            }
            serve_res = axum::serve(self.listener, api::router(appstate)) => {
                if let Err(err) = serve_res {
                    warn!("error serving webapp: {err}");
                }
            }
        }

        info!("aborting the recording task");
        recorder_hdl.abort();
        let _ = recorder_hdl.await;
    }
}
