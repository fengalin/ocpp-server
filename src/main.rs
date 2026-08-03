use anyhow::{Context, bail};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use log::*;
use ocpp_rs::{
    datetime::DateTimeWrapper,
    v16::{
        call::{self, Action, Call},
        call_result::{self, CallResultRaw},
        data_types,
        enums::*,
        parse::{self, Message},
        pending::PendingCalls,
        response_trait::Response,
        typed_call_result::TypedCallResult,
    },
};
use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite as ts};

mod charging_session;
use charging_session::*;

mod measurements;
use measurements::*;

mod schedule;
use schedule::*;

const SOC: Option<f64> = Some(0.36);
const PORT: u16 = 9000;
const HEARTBEAT_INTERVAL_S: u32 = 3600;

#[derive(Debug)]
struct Connection {
    peer: SocketAddr,
    ws_stream: WebSocketStream<TcpStream>,
    connector_id: Option<u32>,
    prepared_first_actions: bool,
    send_action_id: usize,
    pending_response: Option<Message>,
    call_response_tracker: PendingCalls,
    call_queue: VecDeque<Action>,
    charging_session: Option<ChargingSession>,
}

impl Connection {
    fn define_schedule(&self) -> Option<call::SetChargingProfile> {
        const SET_PROFILE: bool = false;
        if !SET_PROFILE {
            return None;
        }

        use chrono::NaiveTime;
        let set_charing_profile = ChargingProfileBuilder::new()
            .add(
                ChargingSchedulePeriodBuild::starting_ending_today(
                    NaiveTime::from_hms_opt(14, 40, 00).unwrap(),
                    NaiveTime::from_hms_opt(16, 28, 00).unwrap(),
                )
                .unwrap(),
            )
            .build();

        // if let Some(ref set_charging_profile) = set_charing_profile {
        //     info!("{} {set_charging_profile:#?}", self.peer);
        //     return None;
        // }

        set_charing_profile
    }

    fn stop_transaction(&self) -> Option<call::RemoteStopTransaction> {
        const STOP_SESSION: bool = false;
        if !STOP_SESSION {
            return None;
        }
        let transaction_id = self.charging_session.as_ref().map(|cs| cs.session_id())?;
        Some(call::RemoteStopTransaction { transaction_id })
    }

    fn prepare_first_actions(&mut self, _connector_id: u32) {
        if let Some(stop_transaction) = self.stop_transaction() {
            self.call_queue
                .push_back(Action::RemoteStopTransaction(stop_transaction));
        }

        if let Some(set_charging_profile) = self.define_schedule() {
            self.call_queue
                .push_back(Action::ClearChargingProfile(call::ClearChargingProfile {
                    id: None,
                    connector_id: None,
                    charging_profile_purpose: None,
                    stack_level: None,
                }));
            self.call_queue
                .push_back(Action::SetChargingProfile(set_charging_profile));
        }

        // self.call_queue
        //     .push_back(Action::GetCompositeSchedule(call::GetCompositeSchedule {
        //         connector_id: _connector_id,
        //         duration: 7 * 24 * 60 * 60,
        //         charging_rate_unit: None,
        //     }));

        // let availability_type = AvailabilityType::Inoperative;
        // // let availability_type = AvailabilityType::Operative;
        // self.call_queue
        //     .push_back(Action::ChangeAvailability(call::ChangeAvailability {
        //         connector_id: _connector_id,
        //         availability_type,
        //     }));

        // self.call_queue
        //     .push_back(Action::GetConfiguration(call::GetConfiguration {
        //         key: None,
        //     }));

        // self.call_queue
        //     .push_back(Action::UnlockConnector(call::UnlockConnector {
        //         connector_id: 1,
        //     }));

        // const ESOL_SERVER_ADDRESS: &str = "wss://ocpp.cpms.esolutionscharging.com/ocpp";
        // self.call_queue
        //     .push_back(Action::ChangeConfiguration(call::ChangeConfiguration {
        //         key: "CS_URL".to_string(),
        //         value: ESOL_SERVER_ADDRESS.to_string(),
        //     }));
        // self.call_queue.push_back(Action::Reset(call::Reset {
        //     reset_type: ResetType::Soft,
        // }));

        self.prepared_first_actions = true;
    }

    fn new(peer: SocketAddr, ws_stream: WebSocketStream<TcpStream>) -> Self {
        Connection {
            peer,
            ws_stream,
            connector_id: None,
            prepared_first_actions: false,
            send_action_id: 0,
            pending_response: None,
            call_response_tracker: PendingCalls::new(),
            call_queue: VecDeque::new(),
            charging_session: ChargingSession::get_last_active(),
        }
    }

    async fn handle_incoming_message(&mut self, msg: &str) -> anyhow::Result<()> {
        // FIXME for every early return, we should enqueue an error Message

        let Ok(msg) = parse::deserialize_to_message(msg) else {
            error!("Failed to deserialize message: {msg}");
            return Ok(());
        };
        match msg {
            Message::Call(call) => self
                .handle_incoming_call(call)
                .await
                .context("incoming call")?,
            Message::CallResult(call_result) => self.handle_incoming_call_result(call_result),
            Message::CallError(err) => error!("{err:?}"),
        }

        Ok(())
    }

    async fn handle_incoming_call(&mut self, call: Call) -> anyhow::Result<()> {
        trace!("{} >> incoming {call:?}", self.peer);

        match call.payload {
            Action::StatusNotification(action) => {
                if action.connector_id != 0 {
                    if !matches!(action.error_code, ChargePointErrorCode::NoError) {
                        warn!(
                            "{} >> connector {}: {:?} {:?}, timestamp: {:?}",
                            self.peer,
                            action.connector_id,
                            action.status,
                            action.error_code,
                            action
                                .timestamp
                                .map(|ts| ts.inner().with_timezone(&chrono::Local)),
                        );
                    } else {
                        info!(
                            "{} >> connector {}: {:?}, timestamp: {:?}",
                            self.peer,
                            action.connector_id,
                            action.status,
                            action
                                .timestamp
                                .map(|ts| ts.inner().with_timezone(&chrono::Local)),
                        );

                        if action.status == ChargePointStatus::Available
                            && let Some(mut cs) = self.charging_session.take()
                        {
                            warn!(
                                "{} ## ending previous active session with id: {} due to connector status",
                                self.peer,
                                cs.session_id(),
                            );
                            cs.stop(
                                Utc::now(),
                                0,
                                "Got connector available while session with still active",
                            );
                        }
                    }
                    if self.connector_id.is_none() {
                        self.connector_id = Some(action.connector_id);
                    }
                }
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::Heartbeat(action) => {
                info!("{} >> incoming {action:?}", self.peer);
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::Heartbeat {
                        current_time: DateTimeWrapper::new(chrono::Utc::now()),
                    },
                );
            }
            Action::BootNotification(action) => {
                info!("{} >> incoming {action:?}", self.peer);
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::BootNotification {
                        current_time: DateTimeWrapper::new(chrono::Utc::now()),
                        interval: HEARTBEAT_INTERVAL_S as _,
                        status: RegistrationStatus::Accepted,
                    },
                );
            }
            Action::SecurityEventNotification(action) => {
                info!("{} >> incoming {action:?}", self.peer);
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::MeterValues(action) => {
                let mut timestamp = None;
                let mut energy = None;
                let mut power = None;
                let meter_val_selection: Vec<_> = action
                    .meter_value
                    .iter()
                    .map(|mv| {
                        timestamp = Some(mv.timestamp.inner());
                        (
                            mv.timestamp.inner().with_timezone(&chrono::Local),
                            mv.sampled_value
                                .iter()
                                .filter_map(|sv| {
                                    use Measurand::*;
                                    match sv.measurand {
                                        Some(PowerOffered) => Some(format!(
                                            "Power Offered: {} kW",
                                            sv.value
                                                .parse::<u64>()
                                                .map_or(f64::NAN, |v| v as f64 / 1_000.0)
                                        )),
                                        Some(PowerActiveImport) => {
                                            power = sv.value.parse::<u64>().ok();
                                            Some(format!(
                                                "Active Power I: {} kW",
                                                sv.value
                                                    .parse::<u64>()
                                                    .map_or(f64::NAN, |v| v as f64 / 1_000.0)
                                            ))
                                        }
                                        Some(EnergyActiveImportRegister) => {
                                            energy = sv.value.parse::<u64>().ok();
                                            Some(format!(
                                                "Active Energy I: {} kWh",
                                                sv.value
                                                    .parse::<u64>()
                                                    .map_or(f64::NAN, |v| v as f64 / 1_000.0)
                                            ))
                                        }
                                        Some(Voltage) if sv.phase == Some(Phase::L1) => {
                                            Some(format!("Voltage L1: {} V", sv.value))
                                        }
                                        Some(Temperature) => {
                                            Some(format!("Temperature: {} °C", sv.value))
                                        }
                                        Some(Frequency) => {
                                            Some(format!("Frequency: {} Hz", sv.value))
                                        }
                                        _ => None,
                                    }
                                })
                                .collect::<Vec<String>>(),
                        )
                    })
                    .collect();

                info!(
                    "{} >> incoming MeterValues {meter_val_selection:?}, transaction id: {:?}",
                    self.peer, action.transaction_id,
                );
                if let Some(((timestamp, energy), cs)) = Option::zip(
                    Option::zip(timestamp, energy),
                    self.charging_session.as_mut(),
                ) {
                    let session_id = cs.session_id();
                    if let Some(transaction_id) = action.transaction_id {
                        if session_id == transaction_id {
                            cs.add_snapshot(timestamp, energy, power.unwrap_or_default());
                        } else {
                            warn!(
                                "{} >> MeterValues transaction id mismatch {transaction_id}, expected {session_id}",
                                self.peer
                            );
                        }
                    } else {
                        info!(
                            "{} >> MeterValues didn't specify transaction id, adding to session {session_id}",
                            self.peer
                        );
                        cs.add_snapshot(timestamp, energy, power.unwrap_or_default());
                    }
                }
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::DataTransfer(action) => {
                let data_trans_selection: Vec<_> = action
                    .data
                    .iter()
                    .map(|d| {
                        let Ok(dpms) = serde_json::from_str::<Vec<Dpm>>(d) else {
                            return vec![DataTransfer {
                                timestamp: "N/A".to_string(),
                                transaction_id: None,
                                sampled_values: vec![d.clone()],
                            }];
                        };
                        dpms.iter()
                            .map(|dpm| DataTransfer {
                                timestamp: dpm.data.timestamp.clone(),
                                transaction_id: dpm.data.transaction_id,
                                sampled_values: dpm
                                    .data
                                    .sampled_value
                                    .iter()
                                    .filter_map(|sv| match sv.measurand.as_str() {
                                        "Power.Active.Import" => Some(format!(
                                            "Active Power I: {} kW, ",
                                            sv.value
                                                .parse::<u64>()
                                                .map_or(f64::NAN, |v| v as f64 / 1_000.0)
                                        )),
                                        "Energy.Active.Import.Register" => Some(format!(
                                            "Active Energy I: {} kWh, ",
                                            sv.value
                                                .parse::<u64>()
                                                .map_or(f64::NAN, |v| v as f64 / 1_000.0)
                                        )),
                                        "Voltage" if sv.phase.as_deref() == Some("L1") => {
                                            Some(format!("Voltage L1: {} V, ", sv.value))
                                        }
                                        _ => None,
                                    })
                                    .collect(),
                            })
                            .collect()
                    })
                    .collect();

                info!(
                    "{} >> incoming Data Transfer {} {data_trans_selection:?}",
                    self.peer,
                    action.message_id.as_ref().unwrap(),
                );
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::DataTransfer {
                        status: DataTransferStatus::Accepted,
                        data: None,
                    },
                );
            }
            Action::StartTransaction(action) => {
                if let Some(mut cs) = self.charging_session.take() {
                    warn!(
                        "{} ## new session ending previous active session with id: {}",
                        self.peer,
                        cs.session_id(),
                    );
                    cs.stop(
                        Utc::now(),
                        0,
                        "Got start transaction while session with still active",
                    );
                }

                let cs = ChargingSession::new(action.timestamp.inner(), action.meter_start, SOC);
                let transaction_id = cs.session_id();
                info!(
                    "{} ## starting transaction with id: {}, timestamp: {}, meter start: {}",
                    self.peer,
                    transaction_id,
                    action.timestamp.inner().with_timezone(&chrono::Local),
                    action.meter_start,
                );
                self.charging_session = Some(cs);
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::StartTransaction {
                        id_tag_info: data_types::IdTagInfo {
                            expiry_date: None,
                            parent_id_tag: None,
                            status: AuthorizationStatus::Accepted,
                        },
                        transaction_id,
                    },
                );
            }
            Action::StopTransaction(action) => {
                if let Some(mut cs) = self.charging_session.take() {
                    let cur_session_id = cs.session_id();
                    if cur_session_id == action.transaction_id {
                        info!(
                            "{} ## transaction with id: {} stopped, timestamp: {}, meter stop: {}, reason: {:?}",
                            self.peer,
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop,
                            action.reason
                        );
                        cs.stop(
                            action.timestamp.inner(),
                            action.meter_stop,
                            action
                                .reason
                                .map_or("UNKNOWN".to_string(), |r| format!("{r:?}")),
                        )
                    } else {
                        warn!(
                            "{} ## transaction with id: {} stopped (expected {cur_session_id}), timestamp: {}, meter stop: {}, reason: {:?}",
                            self.peer,
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop,
                            action.reason
                        );
                        cs.stop(Utc::now(), 0, "Got stop transaction for another session");
                    }
                } else {
                    warn!(
                        "{} ## transaction with id: {} stopped (unexpected), timestamp: {}, meter stop: {}, reason: {:?}",
                        self.peer,
                        action.transaction_id,
                        action.timestamp.inner().with_timezone(&chrono::Local),
                        action.meter_stop,
                        action.reason
                    );
                }
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::StopTransaction { id_tag_info: None },
                );
            }
            Action::Authorize(action) => {
                // FIXME is this even sent with this charging point?
                info!("{} >> incoming {action:?}", self.peer);
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::Authorize {
                        id_tag_info: data_types::IdTagInfo {
                            expiry_date: None,
                            parent_id_tag: None,
                            status: AuthorizationStatus::Accepted,
                        },
                    },
                );
            }
            _ => {
                info!("{} >> incoming {call:?}", self.peer);
            }
        };

        if let Some(pending_response) = self.pending_response.take() {
            trace!("{} << sending response {pending_response:?}", self.peer);
            match parse::serialize_message(&pending_response) {
                Ok(response) => {
                    self.ws_stream
                        .send(ts::Message::Text(response.into()))
                        .await
                        .context("sending response")?;
                }
                Err(err) => {
                    error!("Failed to serialize response: {err}, {pending_response:?}");
                }
            }
        }

        Ok(())
    }

    fn prepare_response<A: Response + std::fmt::Debug>(
        &mut self,
        action: A,
        unique_id: String,
        response: A::ResponseType,
    ) {
        match action.get_response(unique_id, response) {
            Ok(response) => {
                self.pending_response = Some(response);
            }
            Err(err) => {
                error!("Failed to build response: {err} {action:?}");
            }
        }
    }

    fn handle_incoming_call_result(&mut self, call_result: CallResultRaw) {
        debug!("{} >> incoming {call_result:?}", self.peer);

        match self
            .call_response_tracker
            .resolve(call_result)
            .context("resolving call result")
        {
            Ok(TypedCallResult::GetConfiguration(result)) => {
                info!("{} >> incoming {result:#?}", self.peer);
            }
            Ok(TypedCallResult::ClearChargingProfile(result)) => {
                info!("{} >> incoming {result:?}", self.peer);
            }
            Ok(TypedCallResult::GetCompositeSchedule(result)) => {
                info!("{} >> incoming {result:#?}", self.peer);
            }
            Ok(TypedCallResult::ChangeAvailability(result)) => {
                info!("{} >> incoming {result:#?}", self.peer);
            }
            Ok(TypedCallResult::RemoteStartTransaction(result)) => {
                info!("{} >> incoming {result:#?}", self.peer);
            }
            Ok(TypedCallResult::RemoteStopTransaction(result)) => {
                info!("{} >> incoming {result:#?}", self.peer);
            }
            other => {
                info!("{} >> incoming {other:?}", self.peer);
            }
        }
    }

    async fn send_action(&mut self, action: Action) -> anyhow::Result<()> {
        self.send_action_id += 1;
        info!(
            "{} << sending {} {action:?}",
            self.peer, self.send_action_id
        );
        let call = Call::new(format!("{}.occp-server-test", self.send_action_id), action);
        let call = self
            .call_response_tracker
            .send_call(call)
            .context("send_call")?;
        self.ws_stream
            .send(ts::Message::Text(call.into()))
            .await
            .context("ws send")?;

        Ok(())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        loop {
            let Some(msg) = self.ws_stream.next().await else {
                bail!("websocket terminated");
            };

            let msg = msg.inspect_err(|err| match err {
                ts::Error::ConnectionClosed | ts::Error::Protocol(_) | ts::Error::Utf8(_) => (),
                other => error!("Error processing connection: {other}"),
            })?;

            self.handle_incoming_ws_message(msg)
                .await
                .context("handling incoming message")?;

            if !self.prepared_first_actions
                && let Some(connector_id) = self.connector_id
            {
                self.prepare_first_actions(connector_id);
            }

            if let Some(action) = self.call_queue.pop_front()
                && let Err(err) = self.send_action(action).await
            {
                error!("{}: failed to send first action: {err}", self.peer);
            }
        }
    }

    async fn handle_incoming_ws_message(&mut self, msg: ts::Message) -> anyhow::Result<()> {
        match msg {
            ts::Message::Text(text) => {
                self.handle_incoming_message(text.as_str())
                    .await
                    .context("handling incoming ws msg")?;
            }
            ts::Message::Binary(payload) => {
                warn!("{} >> msg bin: {payload:?}", self.peer);
            }
            ts::Message::Ping(payload) => {
                trace!("{} >> ping", self.peer);
                self.ws_stream.send(ts::Message::Pong(payload)).await?;
            }
            ts::Message::Close(reason) => {
                warn!("{} >> websocket closed by peer: {reason:?}", self.peer);
                return Ok(());
            }
            other => {
                warn!("{} >> unhandled websocket message: {other:?}", self.peer);
            }
        }

        Ok(())
    }
}

async fn accept_connection(peer: SocketAddr, stream: TcpStream) {
    let mut connection = Connection::new(peer, accept_async(stream).await.expect("can accept"));
    if let Err(err) = connection.start().await {
        error!("{}: {err:#}", connection.peer);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    // make sure the DB is available
    let _ = &*DATABASE;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bindind to {addr}"))?;
    info!("Listening on: {addr}");

    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr().context("getting peer address")?;
        info!("peer address {peer}");

        tokio::spawn(accept_connection(peer, stream));
    }

    Ok(())
}
