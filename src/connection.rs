use anyhow::{Context, bail};
use chrono::Utc;
use futures::{future::FusedFuture, prelude::*};
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
use std::{collections::VecDeque, net::SocketAddr};
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, tungstenite as ts};

use crate::{
    Bms, ChargingPlan, ChargingSession, ChargingSessionSnapshot, ChargingSessionState,
    measurements::*, schedule,
};

const HEARTBEAT_INTERVAL_S: u32 = 3600;

#[derive(Debug)]
pub struct Connection {
    peer: SocketAddr,
    ws_stream: WebSocketStream<TcpStream>,
    connector_id: Option<u32>,
    bms: Bms,
    last_known_stop_energy: Option<u64>,
    charging_plan: Option<ChargingPlan>,
    prepared_first_actions: bool,
    send_action_id: usize,
    pending_response: Option<Message>,
    call_response_tracker: PendingCalls,
    call_queue: VecDeque<Action>,
    charging_session: Option<ChargingSession>,
}

impl Connection {
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

        if let Some(ref charging_plan) = self.charging_plan {
            self.call_queue
                .push_back(Action::ClearChargingProfile(call::ClearChargingProfile {
                    id: None,
                    connector_id: None,
                    charging_profile_purpose: None,
                    stack_level: None,
                }));

            if let Some(charging_schedule) = charging_plan.to_charging_schedule(&self.bms) {
                self.call_queue.push_back(Action::SetChargingProfile(
                    schedule::SetChargingProfile::builder(charging_schedule).build(),
                ));
            }
        }

        // self.call_queue.push_back(Action::Reset(call::Reset {
        //     reset_type: ResetType::Soft,
        // }));

        // // let availability_type = AvailabilityType::Inoperative;
        // let availability_type = AvailabilityType::Operative;
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

        // self.call_queue
        //     .push_back(Action::ChangeConfiguration(call::ChangeConfiguration {
        //         key: "CS_URL".to_string(),
        //         value: "ws://192.168.1.16:9000".to_string(),
        //     }));
        // self.call_queue.push_back(Action::Reset(call::Reset {
        //     reset_type: ResetType::Soft,
        // }));

        self.prepared_first_actions = true;
    }

    pub fn new(
        peer: SocketAddr,
        ws_stream: WebSocketStream<TcpStream>,
        bms: Bms,
        charging_plan: Option<ChargingPlan>,
        mut last_charging_session: Option<ChargingSession>,
    ) -> Self {
        let last_known_stop_energy = match last_charging_session {
            Some(ref cs) if cs.is_complete() => {
                let last_known_stop_energy = cs.last_energy();
                last_charging_session = None;
                Some(last_known_stop_energy)
            }
            _ => None,
        };

        Connection {
            peer,
            ws_stream,
            connector_id: None,
            bms,
            last_known_stop_energy,
            charging_plan,
            prepared_first_actions: false,
            send_action_id: 0,
            pending_response: None,
            call_response_tracker: PendingCalls::new(),
            call_queue: VecDeque::new(),
            charging_session: last_charging_session,
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

                        match action.status {
                            ChargePointStatus::Available | ChargePointStatus::Finishing => {
                                if let Some(mut cs) = self.charging_session.take()
                                    && !cs.is_complete()
                                {
                                    // when the transaction was stopped by the server
                                    // (SoC cap was reached), the status is Finishing.
                                    // The only way to get it back to a status which
                                    // would allow starting a new session is by unplugging
                                    // the EV first or by rebooting the charging point.
                                    warn!(
                                        "{} ## ending previous active session with id: {} \
                                        due to connector status: {:?}",
                                        self.peer,
                                        cs.session_id(),
                                        action.status,
                                    );
                                    cs.stop(
                                        Utc::now(),
                                        0,
                                        ChargingSessionState::Error(
                                            "Got connector available while session with still active"
                                                .to_string(),
                                        ),
                                    );
                                }
                            }
                            ChargePointStatus::Charging
                            | ChargePointStatus::SuspendedEVSE
                            | ChargePointStatus::Preparing => {
                                // the EV is not charging due to EVSE not providing
                                // energy (e.g. charging period with power limit set to 0)
                                // however, the session can still be restarted
                                if let Some(cs) = self.charging_session.as_mut() {
                                    // we can't ensure this is the same transaction
                                    // and can only hope we got at least one MeterValue
                                    // with the transaction id before getting this
                                    // StatusNotification
                                    cs.set_state(action.status.clone());
                                } else {
                                    // missed the transaction start
                                    warn!("{} ## adding missed charging session", self.peer);
                                    self.check_bms_on_missed_session();

                                    self.charging_session = Some(ChargingSession::with_state(
                                        action.status.clone(),
                                        self.last_known_stop_energy.unwrap_or_default(),
                                        self.bms,
                                        None,
                                        action.timestamp.map(|ts| ts.inner()),
                                    ));
                                }
                            }
                            ChargePointStatus::SuspendedEV => {
                                // FIXME reached 100% => not restarting the session?
                            }
                            ChargePointStatus::Faulted => error!("faulted"),
                            _ => (),
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
                let mut l1_voltage = None;
                let mut temperature = None;
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
                                            l1_voltage = sv.value.parse::<u64>().ok();
                                            Some(format!("Voltage L1: {} V", sv.value))
                                        }
                                        Some(Temperature) => {
                                            temperature = sv.value.parse::<u64>().ok();
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
                ) && let Some(transaction_id) = action.transaction_id
                {
                    let session_id = cs.session_id();
                    if session_id == transaction_id {
                        if energy > cs.last_energy() {
                            let snapshot = ChargingSessionSnapshot::builder(timestamp, energy)
                                .power(power)
                                .l1_voltage(l1_voltage)
                                .temperature(temperature)
                                .build();
                            if self.last_known_stop_energy.is_none()
                                || self.last_known_stop_energy == Some(0)
                            {
                                info!(
                                    "{} ## session {session_id}: current {:.3} kWh (SoC can not be determined)",
                                    self.peer,
                                    energy as f64 / 1_000f64
                                );
                            } else {
                                let soc_progress = cs.add_snapshot(snapshot);
                                if soc_progress.is_complete() && !cs.is_complete() {
                                    info!(
                                        "{} >> Stopping session {session_id}: {soc_progress}",
                                        self.peer
                                    );
                                    cs.set_state(ChargingSessionState::SocCapReached);
                                    self.call_queue.push_back(Action::RemoteStopTransaction(
                                        call::RemoteStopTransaction {
                                            transaction_id: session_id,
                                        },
                                    ));
                                } else {
                                    info!("{} ## session {session_id}: {soc_progress}", self.peer);
                                }
                            }
                        }
                    } else {
                        warn!(
                            "{} >> MeterValues transaction id mismatch {transaction_id}, expected {session_id}",
                            self.peer
                        );
                    }
                } else if let Some(energy) = energy {
                    if let Some(transaction_id) = action.transaction_id {
                        warn!(
                            "{} ## adding missed charging session with transaction id {transaction_id},\
                            last known stop energy: {:.3}",
                            self.peer,
                            self.last_known_stop_energy.unwrap_or_default() as f64 / 1_000f64,
                        );
                        self.check_bms_on_missed_session();

                        self.charging_session = Some(ChargingSession::with_state(
                            ChargingSessionState::Unknown,
                            self.last_known_stop_energy.unwrap_or_default(),
                            self.bms,
                            transaction_id,
                            None,
                        ));
                    } else {
                        // no known session in progress
                        self.update_last_known_stop_energy(energy);
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
                if let Some(mut cs) = self.charging_session.take()
                    && !cs.state().is_complete()
                {
                    warn!(
                        "{} ## new session ending previous session with id: {}, state: {}",
                        self.peer,
                        cs.session_id(),
                        cs.state(),
                    );
                    cs.stop(
                        Utc::now(),
                        0,
                        ChargingSessionState::Error(
                            "Got start transaction while session with still active".to_string(),
                        ),
                    );
                }

                let cs =
                    ChargingSession::new(self.bms, action.timestamp.inner(), action.meter_start);
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
                        cs.stop(action.timestamp.inner(), action.meter_stop, action.reason)
                    } else {
                        warn!(
                            "{} ## transaction with id: {} stopped (expected {cur_session_id}), timestamp: {}, meter stop: {}, reason: {:?}",
                            self.peer,
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop,
                            action.reason
                        );
                        cs.stop(
                            Utc::now(),
                            0,
                            ChargingSessionState::Error(
                                "Got stop transaction for another session".to_string(),
                            ),
                        );
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
                    ChargingSession::save_missing_stopped_session(
                        Some(action.timestamp.inner()),
                        action.reason,
                        action.meter_stop,
                        Some(action.transaction_id),
                    );
                }
                self.update_last_known_stop_energy(action.meter_stop);
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

    fn update_last_known_stop_energy(&mut self, meter_energy: u64) {
        match self.last_known_stop_energy {
            Some(last) if last == meter_energy => (),
            Some(ref mut last) if *last < meter_energy => {
                if self.charging_session.is_none() {
                    // only log when no charging session is in progress
                    info!(
                        "{} ## updating last known stop energy: {:.3} kWh, \
                        previous: {:.3} kWh",
                        self.peer,
                        meter_energy as f64 / 1_000f64,
                        *last as f64 / 1_000f64
                    );
                }
                *last = meter_energy;
            }
            None => {
                info!(
                    "{} ## setting last known stop energy: {:.3} kWh",
                    self.peer,
                    meter_energy as f64 / 1_000f64,
                );
                self.last_known_stop_energy = Some(meter_energy);
            }
            Some(last) => {
                info!(
                    "{} ## ignoring last meter energy: {:.3} kWh, \
                    lower than last known stop energy: {:.3} kWh",
                    self.peer,
                    meter_energy as f64 / 1_000f64,
                    last as f64 / 1_000f64
                );
            }
        }
    }

    fn check_bms_on_missed_session(&self) {
        if self.bms.soc_cap.is_some() {
            if self.last_known_stop_energy.is_none() || self.last_known_stop_energy == Some(0) {
                warn!(
                    "{} ## last stop energy is unknown => \
                    SoC & SoC cap will not have the expected effect",
                    self.peer
                );
            } else {
                warn!(
                    "{} ## ensure the SoC & SoC cap are set as expected",
                    self.peer
                );
            }
        }
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

    pub fn peer(&self) -> &SocketAddr {
        &self.peer
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
                complete => break,
            }
        }

        Ok(())
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
