use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::Context;
use log::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use crate::{CommandToChargingPoint, args, notification::ChargePointNotification};

mod api;
mod ingestor;
mod model;
use model::Characteristics;
mod plan;
mod state;
use state::AppState;

#[derive(Debug)]
pub struct Webserver {
    listener: Option<TcpListener>,
    bms: crate::Bms,
    notif_rx: Option<broadcast::Receiver<ChargePointNotification>>,
    cmd_tx: Option<mpsc::Sender<CommandToChargingPoint>>,
}

impl Webserver {
    pub async fn new(
        args: &args::Args,
        bms: &crate::Bms,
        notif_rx: broadcast::Receiver<ChargePointNotification>,
        cmd_tx: mpsc::Sender<CommandToChargingPoint>,
    ) -> anyhow::Result<Self> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.webserver_port);

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bindind to {addr}"))?;
        info!("Listening on: {addr}");

        Ok(Webserver {
            listener: Some(listener),
            bms: bms.clone(),
            notif_rx: Some(notif_rx),
            cmd_tx: Some(cmd_tx),
        })
    }

    pub async fn into_task(mut self, mut stop_rx: broadcast::Receiver<()>) {
        // FIXME bms os whatever the new name I choose
        let ev_ch = Characteristics::default();

        let appstate = AppState::new(ev_ch, self.cmd_tx.take().unwrap());

        let ingestor_hdl = tokio::spawn(ingestor::into_task(
            appstate.clone(),
            self.bms.clone(),
            self.notif_rx.take().unwrap(),
        ));

        tokio::select! {
            biased;
            _  = stop_rx.recv() => {
                info!("shutting down due to stop request");
            }
            serve_res = axum::serve(self.listener.take().unwrap(), api::router(appstate)) => {
                if let Err(err) = serve_res {
                    warn!("error serving webapp: {err}");
                }
            }
        }

        info!("aborting ingestor");
        ingestor_hdl.abort();
        let _ = ingestor_hdl.await;
    }
}
