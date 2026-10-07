use std::net::{Ipv4Addr, SocketAddrV4};
use std::ops::ControlFlow;

use anyhow::{Context, bail};
use futures::prelude::*;
use log::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite as ts};

use std::time::Duration;

use crate::{
    Bms, ChargingPlan, CommandToChargingPoint, Evse, OcppInterface, args,
    notification::{ChargePointConnectionState, ChargePointNotification},
};

const RECONNECT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct Dispatcher {
    notif_tx: broadcast::Sender<ChargePointNotification>,
    ws_stream: Option<WebSocketStream<TcpStream>>,
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
        notif_tx: broadcast::Sender<ChargePointNotification>,
        charging_plan: Option<ChargingPlan>,
    ) -> anyhow::Result<Self> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bindind to {addr}"))?;
        info!("Listening on: {addr}");

        Ok(Dispatcher {
            notif_tx: notif_tx.clone(),
            ws_stream: None,
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

        if let Some(mut ws_stream) = self.ws_stream.take()
            && let Err(err) = ws_stream.close(None).await.context("closing websocket")
        {
            error!("{err:#}");
        }
    }

    pub async fn accept(&mut self) -> anyhow::Result<()> {
        let mut log_accept_failure = true;

        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    log_accept_failure = true;

                    if let Err(err) = self.listen(stream).await {
                        warn!("error listening to OCPP ws: {err}");
                        self.notif_error();
                    }
                }
                Err(err) => {
                    if log_accept_failure {
                        log_accept_failure = false;
                        warn!("error accepting OCPP ws: {err}");
                        self.notif_error();
                    }

                    tokio::time::sleep(RECONNECT).await;
                }
            }
        }
    }

    pub async fn listen(&mut self, stream: TcpStream) -> anyhow::Result<()> {
        let peer = stream.peer_addr().context("getting peer address")?;
        self.ws_stream = Some(accept_async(stream).await.context("accepting ws stream")?);

        info!("peer address {peer}");

        // FIXME might want to refresh EVSE state

        if let Some(command) = self.command.take() {
            use args::Command::*;
            match command {
                GetConfiguration => {
                    self.ocpp_if
                        .push_command(CommandToChargingPoint::GetConfiguration);
                }
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
                SetMeterValueSampleInterval(val) => {
                    self.ocpp_if
                        .push_command(CommandToChargingPoint::SetMeterValueSampleInterval(
                            val.interval,
                        ));
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

            let Some(msg) = self.ws_stream.as_mut().unwrap().next().await else {
                bail!("websocket terminated");
            };

            let msg = msg.inspect_err(|err| match err {
                ts::Error::ConnectionClosed | ts::Error::Protocol(_) | ts::Error::Utf8(_) => (),
                other => error!("cp websocket error: {other}"),
            })?;

            if self
                .handle_incoming_ws_message(msg)
                .await
                .context("handling incoming cp message")?
                .is_break()
            {
                warn!("stopping listener loop further to message handling");
                break;
            }

            for call in self.ocpp_if.pending_calls(&mut self.evse) {
                trace!("<< sending {call:?}");
                self.ws_stream
                    .as_mut()
                    .unwrap()
                    .send(ts::Message::Text(call.into()))
                    .await
                    .context("ws send")?;
            }
        }

        Ok(())
    }

    async fn handle_incoming_ws_message(
        &mut self,
        msg: ts::Message,
    ) -> anyhow::Result<ControlFlow<()>> {
        match msg {
            ts::Message::Text(text) => {
                if let Some(response) = self
                    .ocpp_if
                    .handle_incoming_message(&mut self.evse, text.as_str())
                {
                    trace!("<< sending response {response:?}");
                    self.ws_stream
                        .as_mut()
                        .unwrap()
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
                warn!(">> websocket closed by peer: {reason:?}");
                // forget it: we don't need to explicitely close it now
                self.ws_stream = None;
                return Ok(ControlFlow::Break(()));
            }
            other => {
                warn!(">> unhandled websocket message: {other:?}");
            }
        }

        Ok(ControlFlow::Continue(()))
    }

    fn notif_error(&self) {
        trace!("notifiying error");
        if let Err(err) = self.notif_tx.send(ChargePointNotification::Connection(
            ChargePointConnectionState::Error,
        )) {
            error!("error sending notification: {err}");
        }
    }
}
