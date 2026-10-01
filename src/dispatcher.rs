use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context, bail};
use futures::prelude::*;
use log::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite as ts};

use crate::{Bms, ChargingPlan, CommandToChargingPoint, Evse, OcppInterface, args, notification};

const RECONNECT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct Dispatcher {
    notif_tx: broadcast::Sender<notification::ChargeState>,
    command: Option<args::Command>,
    charging_plan: Option<ChargingPlan>,
    listener: TcpListener,
    ocpp_if: OcppInterface,
    evse: Evse,
}

impl Dispatcher {
    pub async fn new(
        bms: Bms,
        args: &args::Args,
        notif_tx: broadcast::Sender<notification::ChargeState>,
        charging_plan: Option<ChargingPlan>,
    ) -> anyhow::Result<Self> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bindind to {addr}"))?;
        info!("Listening on: {addr}");

        Ok(Dispatcher {
            notif_tx: notif_tx.clone(),
            command: args.command.clone(),
            charging_plan,
            listener,
            ocpp_if: OcppInterface::new(notif_tx.clone()),
            evse: Evse::new(bms, notif_tx),
        })
    }

    pub async fn into_task(mut self, mut stop_rx: broadcast::Receiver<()>) {
        tokio::select! {
            biased;
            _  = stop_rx.recv() => {
                info!("shutting down due to stop request");
            }
            _ = self.accept() => (),
        }
    }

    pub async fn accept(&mut self) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    if let Err(err) = self.listen(stream).await {
                        warn!("error listening to OCPP ws: {err}");
                        self.notif_error();
                    }
                }
                Err(err) => {
                    warn!("error accepting OCPP ws: {err}");
                    self.notif_error();
                }
            }

            tokio::time::sleep(RECONNECT).await;
        }
    }

    pub async fn listen(&mut self, stream: TcpStream) -> anyhow::Result<()> {
        let peer = stream.peer_addr().context("getting peer address")?;
        let mut ws_stream = accept_async(stream).await.context("accepting ws stream")?;

        info!("peer address {peer}");

        // FIXME might want to refresh EVSE state

        if let Some(command) = self.command.take() {
            use args::Command::*;
            match command {
                Run => {
                    if let Some(charging_plan) = self.charging_plan.take() {
                        self.evse.set_charging_plan(charging_plan);
                    } else {
                        // no charging plan specified, re-apply last schedule if any,
                        // in case it was removed (e.g. due to a charging point reboot)
                        // FIXME only do this if there's an outstanding charging period
                        // self.evse.refresh_charging_schedule();
                    }
                }
                StopSession => self.evse.stop_current_session(),
                Reboot => {
                    self.evse.permanent_0w_set();
                    self.ocpp_if.push_command(CommandToChargingPoint::Reboot);
                }
                SetServerIp(ip_address) => {
                    let server_ip = ip_address.get_ip_address().expect("checked by caller");
                    self.evse.permanent_0w_set();
                    self.ocpp_if
                        .push_command(CommandToChargingPoint::SetServerAddress(format!(
                            "ws://{server_ip}:9000"
                        )));
                }
            };
        }

        loop {
            trace!("## dispatcher loop iter");

            tokio::select! {
                biased;

                recv_res = ws_stream.next() => {
                    let Some(msg) = recv_res else {
                        bail!("websocket terminated");
                    };

                    let msg = msg.inspect_err(|err| match err {
                        ts::Error::ConnectionClosed | ts::Error::Protocol(_) | ts::Error::Utf8(_) => (),
                        other => error!("cp websocket error: {other}"),
                    })?;

                    self.handle_incoming_ws_message(&mut ws_stream, msg)
                        .await
                        .context("handling incoming cp message")?;

                    for call in self.ocpp_if.pending_calls(&mut self.evse) {
                        trace!("<< sending {call:?}");
                        ws_stream
                            .send(ts::Message::Text(call.into()))
                            .await
                            .context("ws send")?;
                    }
                }
            }
        }
    }

    async fn handle_incoming_ws_message(
        &mut self,
        ws_stream: &mut WebSocketStream<TcpStream>,
        msg: ts::Message,
    ) -> anyhow::Result<()> {
        match msg {
            ts::Message::Text(text) => {
                if let Some(response) = self
                    .ocpp_if
                    .handle_incoming_message(&mut self.evse, text.as_str())
                {
                    trace!("<< sending response {response:?}");
                    ws_stream
                        .send(ts::Message::Text(response.into()))
                        .await
                        .context("sending response")?;
                }
            }
            ts::Message::Binary(payload) => {
                warn!(">> msg bin: {payload:?}");
            }
            ts::Message::Ping(_) => trace!(">> ping"),
            ts::Message::Close(reason) => {
                bail!(">> websocket closed by peer: {reason:?}");
            }
            other => {
                warn!(">> unhandled websocket message: {other:?}");
            }
        }

        Ok(())
    }

    fn notif_error(&self) {
        trace!("notifiying error");
        if let Err(err) = self.notif_tx.send(notification::ChargeState::Error) {
            error!("error sending notification: {err}");
        }
    }
}
