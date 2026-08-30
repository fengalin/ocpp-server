use anyhow::{Context, bail};
use futures::{future::FusedFuture, prelude::*};
use log::*;
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, tungstenite as ts};

use crate::{ChargingPlan, CommandToChargingPoint, Evse, OcppInterface, args};

#[derive(Debug)]
pub struct Dispatcher {
    ws_stream: WebSocketStream<TcpStream>,
    evse: Evse,
    ocpp_if: OcppInterface,
}

impl Dispatcher {
    pub fn new(
        ws_stream: WebSocketStream<TcpStream>,
        mut evse: Evse,
        command: args::Command,
        charging_plan: Option<ChargingPlan>,
    ) -> Self {
        use args::Command::*;
        let mut ocpp_if = OcppInterface::new();
        match command {
            Run => {
                if let Some(charging_plan) = charging_plan {
                    evse.set_charging_plan(charging_plan);
                } else {
                    // no charging plan specified, re-apply last schedule if any,
                    // in case it was removed (e.g. due to a charging point reboot)
                    evse.refresh_charging_schedule();
                }
            }
            StopSession => {
                evse.stop_current_session();
            }
            Reboot => {
                evse.permanent_0w_set();
                ocpp_if.push_command(CommandToChargingPoint::Reboot);
            }
            SetServerIp(ip_address) => {
                let server_ip = ip_address.get_ip_address().expect("checked by caller");
                evse.permanent_0w_set();
                ocpp_if.push_command(CommandToChargingPoint::SetServerAddress(format!(
                    "ws://{server_ip}:9000"
                )));
            }
        };

        Dispatcher {
            ws_stream,
            evse,
            ocpp_if,
        }
    }

    pub async fn run_loop(
        &mut self,
        mut ctrl_c: std::pin::Pin<&mut impl FusedFuture<Output = Result<(), std::io::Error>>>,
    ) -> anyhow::Result<()> {
        loop {
            futures::select_biased! {
                _ = ctrl_c => {
                    warn!("shutting down due to SIGINT");
                    self.ws_stream.close(None).await.context("closing websocket")?;
                    let recv_res = self.ws_stream.next().await;
                    info!("client replied {recv_res:?}");
                    return Ok(());
                }
                recv_res = self.ws_stream.next() => {
                    let Some(msg) = recv_res else {
                        bail!("websocket terminated");
                    };

                    let msg = msg.inspect_err(|err| match err {
                        ts::Error::ConnectionClosed | ts::Error::Protocol(_) | ts::Error::Utf8(_) => (),
                        other => error!("Error processing connection: {other}"),
                    })?;

                    self.handle_incoming_ws_message(msg)
                        .await
                        .context("handling incoming message")?;

                    for call in self.ocpp_if.pending_calls(&mut self.evse) {
                        if let Err(err) = self.ws_stream
                            .send(ts::Message::Text(call.into()))
                            .await
                            .context("ws send") {
                                error!("error sending call: {err}");
                        }
                    }
                }
                complete => break,
            }
        }

        Ok(())
    }

    async fn handle_incoming_ws_message(&mut self, msg: ts::Message) -> anyhow::Result<()> {
        match msg {
            ts::Message::Text(text) => {
                if let Some(response) = self
                    .ocpp_if
                    .handle_incoming_message(&mut self.evse, text.as_str())
                {
                    self.ws_stream
                        .send(ts::Message::Text(response.into()))
                        .await
                        .context("sending response")?;
                }
            }
            ts::Message::Binary(payload) => {
                warn!(">> msg bin: {payload:?}");
            }
            ts::Message::Ping(payload) => {
                trace!(">> ping");
                self.ws_stream.send(ts::Message::Pong(payload)).await?;
            }
            ts::Message::Close(reason) => {
                warn!(">> websocket closed by peer: {reason:?}");
                return Ok(());
            }
            other => {
                warn!(">> unhandled websocket message: {other:?}");
            }
        }

        Ok(())
    }
}
