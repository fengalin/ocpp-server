use anyhow::{Context, bail};
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

mod measurements;
use measurements::*;

mod schedule;
use schedule::*;

const PORT: u16 = 9000;
const HEARTBEAT_INTERVAL_S: u32 = 3600;

struct Connection {
    peer: SocketAddr,
    ws_stream: WebSocketStream<TcpStream>,
    connector_id: Option<u32>,
    prepared_first_actions: bool,
    send_action_id: usize,
    pending_response: Option<Message>,
    call_response_tracker: PendingCalls,
    call_queue: VecDeque<Action>,
    transaction_id: i32,
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
                    NaiveTime::from_hms_opt(14, 20, 00).unwrap(),
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
        const TRANSACTION_ID: i32 = 0;
        if TRANSACTION_ID == 0 {
            return None;
        }
        Some(call::RemoteStopTransaction {
            transaction_id: TRANSACTION_ID,
        })
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
            transaction_id: 0,
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
        debug!("{} >> incoming {call:?}", self.peer);

        match call.payload {
            Action::StatusNotification(action) => {
                debug!("{} >> incoming {action:?}", self.peer);
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
                let meter_val_selection: Vec<_> = action
                    .meter_value
                    .iter()
                    .map(|mv| {
                        (
                            mv.timestamp.inner().with_timezone(&chrono::Local),
                            mv.sampled_value
                                .iter()
                                .filter_map(|sv| {
                                    use Measurand::*;
                                    match sv.measurand {
                                        Some(PowerOffered) => Some(format!(
                                            "Power Offered: {} {:?}",
                                            if let Ok(val) = sv.value.parse::<i32>()
                                                && (val < 0 || val == i32::MAX)
                                            {
                                                "N/A"
                                            } else {
                                                sv.value.as_str()
                                            },
                                            sv.unit.as_ref().unwrap()
                                        )),
                                        Some(PowerActiveImport) => Some(format!(
                                            "Active Power I: {} {:?}",
                                            sv.value,
                                            sv.unit.as_ref().unwrap()
                                        )),
                                        Some(EnergyActiveImportRegister) => Some(format!(
                                            "Active Energy I: {} {:?}",
                                            sv.value,
                                            sv.unit.as_ref().unwrap()
                                        )),
                                        Some(Temperature) => Some(format!(
                                            "Temperature: {} {:?}",
                                            sv.value,
                                            sv.unit.as_ref().unwrap()
                                        )),
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
                    "{} >> incoming MeterValues {meter_val_selection:#?}",
                    self.peer
                );
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::DataTransfer(action) => {
                let data_trans_selection: Vec<_> = action
                    .data
                    .iter()
                    .map(|d| {
                        let Ok(dpms) = serde_json::from_str::<Vec<Dpm>>(&d) else {
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
                                            "Active Power I: {} {:?}, ",
                                            sv.value, sv.unit,
                                        )),
                                        "Energy.Active.Import.Register" => Some(format!(
                                            "Active Energy I: {} {:?}, ",
                                            sv.value, sv.unit,
                                        )),
                                        _ => None,
                                    })
                                    .collect(),
                            })
                            .collect()
                    })
                    .collect();

                info!(
                    "{} >> incoming Data Transfer {} {data_trans_selection:#?}",
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
                self.transaction_id += 1;
                info!(
                    "{} ## starting transaction with id: {}, timestamp: {}, meter start: {}",
                    self.peer,
                    self.transaction_id,
                    action.timestamp.inner().with_timezone(&chrono::Local),
                    action.meter_start,
                );
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::StartTransaction {
                        id_tag_info: data_types::IdTagInfo {
                            expiry_date: None,
                            parent_id_tag: None,
                            status: AuthorizationStatus::Accepted,
                        },
                        transaction_id: self.transaction_id,
                    },
                );
            }
            Action::StopTransaction(action) => {
                info!(
                    "{} ## transaction with id: {} stopped, timestamp: {}, meter stop: {}, reason: {:?}",
                    self.peer,
                    self.transaction_id,
                    action.timestamp.inner().with_timezone(&chrono::Local),
                    action.meter_stop,
                    action.reason
                );
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
            _ => (),
        };

        if let Some(pending_response) = self.pending_response.take() {
            debug!("{} << sending response {pending_response:?}", self.peer);
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

            if let Some(action) = self.call_queue.pop_front() {
                if let Err(err) = self.send_action(action).await {
                    error!("{}: failed to send first action: {err}", self.peer);
                }
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
                debug!("{} >> ping", self.peer);
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
