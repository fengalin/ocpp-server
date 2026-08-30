use anyhow::{Context, bail};
use futures::{future::FusedFuture, prelude::*};
use log::*;
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, tungstenite as ts};

use crate::{Bms, ChargingPlan, ChargingSchedule, ChargingSession, Evse, args};

#[derive(Debug)]
pub struct Connection {
    ws_stream: WebSocketStream<TcpStream>,
    evse: Evse,
}

impl Connection {
    // FIXME move Evse construction to caller
    pub fn new(
        ws_stream: WebSocketStream<TcpStream>,
        bms: Bms,
        charging_plan: Option<ChargingPlan>,
        last_charging_session: Option<ChargingSession>,
        last_charging_schedule: Option<ChargingSchedule>,
        command: args::Command,
    ) -> Self {
        Connection {
            ws_stream,
            evse: Evse::new(
                bms,
                charging_plan,
                last_charging_session,
                last_charging_schedule,
                command,
            ),
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

                    while let Some(call) = self.evse.pop_call() {
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
                if let Some(response) = self.evse.handle_incoming_message(text.as_str()) {
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
