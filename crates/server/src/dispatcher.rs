use std::net::{Ipv4Addr, SocketAddrV4};
use std::ops::ControlFlow;

use anyhow::{Context, bail};
use futures::prelude::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite as ts};
use tracing::*;

use std::collections::VecDeque;
use std::time::Duration;

use charge_system_common_model::bms::Bms;
use charge_system_common_model::notification::{ChargeSystemConnectionState, Notification};

use crate::{CommandToChargingPoint, Evse, OcppInterface, args};

const RECONNECT: Duration = Duration::from_secs(5);
/// Bound on the websocket handshake: a client that connects but never
/// completes the upgrade must not squat the single CP slot forever
/// (see `accept`).
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-attempt bound on websocket writes. A write that times out may have
/// left a partial frame on the wire, so the stream is unusable afterwards:
/// after `WS_WRITE_ATTEMPTS` failed attempts the connection is dropped and
/// the undelivered calls re-queued for the next connection.
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const WS_WRITE_ATTEMPTS: usize = 3;
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Dispatcher {
    notif_tx: broadcast::Sender<Notification>,
    cmd_rx: mpsc::Receiver<CommandToChargingPoint>,
    ws_stream: Option<WebSocketStream<TcpStream>>,
    listener: TcpListener,
    ocpp_if: OcppInterface,
    evse: Evse,
    /// Read timeout on the CP websocket (any frame resets it; the CP pings
    /// every 40 s). Configurable via `--ocpp-read-timeout`.
    read_timeout: Duration,
    /// Calls that couldn't be written before the previous connection died;
    /// re-sent first once the CP reconnects.
    retry_calls: VecDeque<String>,
}

impl Dispatcher {
    pub async fn new(
        bms: Bms,
        args: &args::Args,
        notif_tx: broadcast::Sender<Notification>,
        cmd_rx: mpsc::Receiver<CommandToChargingPoint>,
    ) -> anyhow::Result<Self> {
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, args.ocpp_port);

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding to {addr}"))?;
        info!("Listening on: {addr}");

        Ok(Dispatcher {
            notif_tx: notif_tx.clone(),
            cmd_rx,
            ws_stream: None,
            listener,
            ocpp_if: OcppInterface::new(notif_tx.clone()),
            evse: Evse::new(bms, notif_tx),
            read_timeout: Duration::from_secs(args.ocpp_read_timeout as u64),
            retry_calls: VecDeque::new(),
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
            && let Err(err) = timeout(WS_CLOSE_TIMEOUT, ws_stream.close(None))
                .await
                .context("closing websocket")
        {
            error!("{err:#}");
        }
    }

    pub async fn accept(&mut self) -> anyhow::Result<()> {
        // The server drives a single charging point: connections are handled
        // serially, one at a time — a CP connecting while another one is active
        // simply waits in the TCP backlog until `listen` returns.
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
        self.ws_stream = Some(
            timeout(WS_HANDSHAKE_TIMEOUT, accept_async(stream))
                .await
                .context("timed out waiting for the ws handshake")?
                .context("accepting ws stream")?,
        );

        info!("peer address {peer}");

        loop {
            trace!("## dispatcher loop iter");

            tokio::select! {
                cmd_res = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd_res else {
                        bail!("cmd chan terminated");
                    };

                    info!("pushing command {cmd:?}");
                    match cmd {
                        // Local EVSE state updates, no OCPP call involved
                        // (applied the same way as the CLI does).
                        CommandToChargingPoint::SetInitialSoC(soc) => {
                            self.evse.set_initial_soc(soc);
                        }
                        CommandToChargingPoint::SetChargingPlan(charging_plan) => {
                            self.evse.set_charging_plan(charging_plan);
                        }
                        // OCPP commands to be sent to the charging point
                        other => self.ocpp_if.push_command(other),
                    }
                }
                ws_msg_res = timeout(self.read_timeout, self.ws_stream.as_mut().unwrap().next()) => {
                    let msg = match ws_msg_res {
                        // No frame (ping, meter value, ...) within the read
                        // timeout: the CP pings every 40 s, so this means the
                        // connection is dead — drop it and go back to accepting.
                        Err(_) => {
                            error!(
                                timeout_secs = self.read_timeout.as_secs(),
                                "no message from the CP within the read timeout, dropping the connection"
                            );
                            bail!("cp read timeout ({:?})", self.read_timeout);
                        }
                        Ok(None) => bail!("websocket terminated"),
                        Ok(Some(msg)) => msg.inspect_err(|err| match err {
                            ts::Error::ConnectionClosed | ts::Error::Protocol(_) | ts::Error::Utf8(_) => (),
                            other => error!("cp websocket error: {other}"),
                        })?,
                    };

                    if self
                        .handle_incoming_ws_message(msg)
                        .await
                        .context("handling incoming cp message")?
                        .is_break()
                    {
                        warn!("stopping listener loop further to message handling");
                        break;
                    }
                }
            }

            // Calls re-queued after a failed write go out first; a fresh ws
            // connection resets the OCPP message-id scope, so re-sending a
            // previously assigned id is valid.
            let mut calls: VecDeque<String> = self
                .retry_calls
                .drain(..)
                .chain(self.ocpp_if.pending_calls(&mut self.evse))
                .collect();
            while let Some(call) = calls.pop_front() {
                trace!("<< sending {call:?}");
                if let Err(err) = send_ws_with_retry(
                    self.ws_stream.as_mut().unwrap(),
                    ts::Message::Text(call.clone().into()),
                    "pending call",
                )
                .await
                {
                    // A timed-out write may have left a partial frame on the
                    // wire: the stream is unusable, so re-queue the undelivered
                    // calls — they'll go out again once the CP reconnects.
                    self.retry_calls = std::iter::once(call).chain(calls).collect();
                    return Err(err).context("ws send");
                }
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
                    send_ws_with_retry(
                        self.ws_stream.as_mut().unwrap(),
                        ts::Message::Text(response.into()),
                        "call response",
                    )
                    .await
                    .context("sending response")?;
                }
            }
            ts::Message::Binary(payload) => {
                warn!(">> bin msg: {payload:?}");
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
        if let Err(err) = self
            .notif_tx
            .send(Notification::Connection(ChargeSystemConnectionState::Error))
        {
            error!("error sending notification: {err}");
        }
    }
}

/// Send a websocket message, retrying a bounded number of times: a write can
/// stall when the CP stops reading (its TCP buffer is full). A timed-out
/// write may have left a partial frame on the wire, so after the last attempt
/// the caller must drop the connection rather than reuse the stream.
async fn send_ws_with_retry(
    ws_stream: &mut WebSocketStream<TcpStream>,
    msg: ts::Message,
    what: &str,
) -> anyhow::Result<()> {
    for attempt in 1..=WS_WRITE_ATTEMPTS {
        match timeout(WS_WRITE_TIMEOUT, ws_stream.send(msg.clone())).await {
            Ok(Ok(())) => return Ok(()),
            // a real ws error won't fix itself by retrying
            Ok(Err(err)) => return Err(err).context(format!("ws send ({what})")),
            Err(_) => {
                warn!(
                    attempt,
                    attempts = WS_WRITE_ATTEMPTS,
                    what,
                    "ws write timed out, retrying"
                );
            }
        }
    }
    bail!("ws write timed out after {WS_WRITE_ATTEMPTS} attempts ({what})")
}
