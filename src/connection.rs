use anyhow::{Context, bail};
use chrono::Utc;
use futures::{future::FusedFuture, prelude::*};
use log::*;
use ocpp_rs::{
    datetime::DateTimeWrapper,
    v16::{
        self,
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
use std::collections::VecDeque;
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, tungstenite as ts};

use crate::{
    Bms, ChargingPlan, ChargingSchedule, ChargingSession, ChargingSessionSnapshot,
    ChargingSessionState, Database, args, measurements::*, schedule,
};

const HEARTBEAT_INTERVAL_S: u32 = 3600;

#[derive(Debug)]
pub struct Connection {
    ws_stream: WebSocketStream<TcpStream>,
    connector_id: Option<u32>,
    bms: Bms,
    last_known_stop_energy: Option<u64>,
    last_meter_values: Option<MeterValueSelection>,
    send_action_id: usize,
    pending_response: Option<Message>,
    call_response_tracker: PendingCalls,
    call_queue: VecDeque<Action>,
    charging_session: Option<ChargingSession>,
    charging_schedule: Option<ChargingSchedule>,
}

impl Connection {
    // fn prepare_first_actions(&mut self, _connector_id: u32) {
    // self.call_queue
    //     .push_back(Action::ChangeConfiguration(call::ChangeConfiguration {
    //         key: "CS_URL".to_string(),
    //         value: "ws://192.168.1.16:9000".to_string(),
    //     }));
    // self.call_queue.push_back(Action::Reset(call::Reset {
    //     reset_type: ResetType::Soft,
    // }));
    // }

    pub fn new(
        ws_stream: WebSocketStream<TcpStream>,
        bms: Bms,
        charging_plan: Option<ChargingPlan>,
        last_charging_session: Option<ChargingSession>,
        mut last_charging_schedule: Option<ChargingSchedule>,
        command: args::Command,
    ) -> Self {
        let mut this = Connection {
            ws_stream,
            connector_id: None,
            bms,
            last_known_stop_energy: None,
            last_meter_values: None,
            send_action_id: 0,
            pending_response: None,
            call_response_tracker: PendingCalls::new(),
            call_queue: VecDeque::new(),
            charging_session: None,
            charging_schedule: None,
        };

        if let Some(ref cs) = last_charging_session
            && cs.is_complete()
        {
            this.last_known_stop_energy = Some(cs.last_energy());
        } else {
            // FIXME is last_charging_session is some (and not complete)
            // init last_known_stop_energy with the energy of the first snapshot
            // or the initial_energy
            this.charging_session = last_charging_session;
        }

        if let Some(schedule) = last_charging_schedule.take()
            && schedule.is_active()
        {
            this.charging_schedule = Some(schedule);
        }

        use args::Command::*;
        match command {
            Run => {
                if let Some(charging_plan) = charging_plan
                    && let Some(charging_schedule) = charging_plan.to_charging_schedule(&this.bms)
                {
                    this.push_charging_schedule(charging_schedule);
                }
            }
            StopSession => {
                let cs = this.charging_session.as_ref().expect("checked by caller");
                this.call_queue.push_back(Action::RemoteStopTransaction(
                    call::RemoteStopTransaction {
                        // FIXME use transaction id & fallback to session_id
                        // if not defined
                        transaction_id: cs.session_id(),
                    },
                ));
            }
            Reset => {
                // After a reset, the EVSE starts the last chaging plan that
                // was set, regardless of the moment it was supposed to start.
                // Set a 0 W permanent limit to make sure we don't start
                // charging unexpectedly.
                this.push_charging_schedule(ChargingSchedule::new());

                this.call_queue.push_back(Action::Reset(call::Reset {
                    reset_type: ResetType::Soft,
                }));
            }
        }

        this
    }

    fn push_charging_schedule(&mut self, mut schedule: ChargingSchedule) {
        if let Some(mut cur_schedule) = self.charging_schedule.take() {
            cur_schedule.inactivate();
        }

        self.call_queue
            .push_back(Action::ClearChargingProfile(call::ClearChargingProfile {
                id: None,
                connector_id: None,
                charging_profile_purpose: None,
                stack_level: None,
            }));

        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule.clone());

        self.call_queue.push_back(Action::SetChargingProfile(
            schedule::SetChargingProfile::builder(schedule).build(),
        ));
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
        trace!(">> incoming {call:?}");

        match call.payload {
            Action::StatusNotification(action) => {
                if action.connector_id != 0 {
                    if !matches!(action.error_code, ChargePointErrorCode::NoError) {
                        warn!(
                            ">> connector {}: {:?} {:?}, timestamp: {:?}",
                            action.connector_id,
                            action.status,
                            action.error_code,
                            action
                                .timestamp
                                .map(|ts| ts.inner().with_timezone(&chrono::Local)),
                        );
                    } else {
                        info!(
                            ">> connector {}: {:?}, timestamp: {:?}",
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
                                        "## ending previous active session with id: {} \
                                        due to connector status: {:?}",
                                        cs.session_id(),
                                        action.status,
                                    );
                                    cs.stop(
                                        Utc::now(),
                                        0,
                                        ChargingSessionState::Error(
                                            "Got connector available while session was still active"
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
                                    warn!("## adding missed charging session");
                                    self.check_bms_on_missed_session();

                                    self.charging_session = Some(ChargingSession::with_state(
                                        self.bms.clone(),
                                        action.status.clone(),
                                        self.last_known_stop_energy.unwrap_or_default(),
                                        None,
                                        action.timestamp.map(|ts| ts.inner()),
                                    ));
                                }
                            }
                            ChargePointStatus::SuspendedEV | ChargePointStatus::Faulted => {
                                // FIXME reached 100% => not restarting the session?
                                if let Some(mut cs) = self.charging_session.take() {
                                    cs.set_state(action.status.clone());
                                }
                            }
                            ChargePointStatus::Unavailable | ChargePointStatus::Reserved => (),
                        }
                    }
                    if self.connector_id.is_none() {
                        self.connector_id = Some(action.connector_id);
                    }
                }
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::Heartbeat(action) => {
                info!(">> incoming {action:?}");
                self.prepare_response(
                    action,
                    call.unique_id,
                    call_result::Heartbeat {
                        current_time: DateTimeWrapper::new(chrono::Utc::now()),
                    },
                );
            }
            Action::BootNotification(action) => {
                info!(">> incoming {action:?}");
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
                info!(">> incoming {action:?}");
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::MeterValues(action) => {
                let mut meter_val_selection: Vec<_> = action
                    .meter_value
                    .iter()
                    .map(MeterValueSelection::from)
                    .collect();

                // FIXME check if we need to handle possibly multiple items
                if meter_val_selection.len() > 1 {
                    info!(
                        ">> MeterValues {meter_val_selection:?}, transaction id: {:?}",
                        action.transaction_id,
                    );
                }

                if let Some(mv) = meter_val_selection.pop() {
                    if self
                        .last_meter_values
                        .as_ref()
                        .is_some_and(|last_mv| *last_mv != mv)
                        || self.last_meter_values.is_none()
                    {
                        info!(
                            ">> MeterValues {mv}, transaction id: {:?}",
                            action.transaction_id,
                        );
                        self.last_meter_values = Some(mv.clone());
                    }

                    self.handle_meter_value(mv, action.transaction_id);
                }

                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::DataTransfer(action) => {
                for d in action.data.iter() {
                    let Ok(dpm_values) = serde_json::from_str::<Vec<Dpm>>(d) else {
                        continue;
                    };

                    for dpm_value in dpm_values {
                        info!(
                            ">> DPM data: {} {}",
                            action.message_id.as_ref().unwrap(),
                            DpmSelection::from(dpm_value),
                        );
                    }
                }

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
                        "## new session ending previous session with id: {}, state: {}",
                        cs.session_id(),
                        cs.state(),
                    );
                    cs.stop(
                        Utc::now(),
                        0,
                        ChargingSessionState::Error(
                            "Got start transaction while session was still active".to_string(),
                        ),
                    );
                }

                let cs = ChargingSession::new(
                    self.bms.clone(),
                    action.timestamp.inner(),
                    action.meter_start,
                );
                let transaction_id = cs.session_id();
                info!(
                    "## starting transaction with id: {}, timestamp: {}, meter start: {}",
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
                            "## transaction with id: {} stopped, timestamp: {}, meter stop: {}, reason: {:?}",
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop,
                            action.reason
                        );
                        cs.stop(action.timestamp.inner(), action.meter_stop, action.reason)
                    } else {
                        warn!(
                            "## transaction with id: {} stopped (expected {cur_session_id}), timestamp: {}, meter stop: {}, reason: {:?}",
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
                        "## transaction with id: {} stopped (unexpected), timestamp: {}, meter stop: {}, reason: {:?}",
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
                info!(">> incoming {action:?}");
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
                info!(">> incoming {call:?}");
            }
        };

        if let Some(pending_response) = self.pending_response.take() {
            trace!("<< sending response {pending_response:?}");
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

    fn handle_meter_value(&mut self, mv: MeterValueSelection, transaction_id: Option<i32>) {
        if let Some((energy, cs)) =
            Option::zip(mv.active_energy_import, self.charging_session.as_mut())
            && let Some(transaction_id) = transaction_id
        {
            let session_id = cs.session_id();
            if session_id == transaction_id {
                if energy > cs.last_energy() {
                    let snapshot = ChargingSessionSnapshot::builder(mv.timestamp, energy)
                        .power(mv.active_power_import)
                        .l1_voltage(mv.voltage_l1)
                        .temperature(mv.temperature)
                        .build();

                    if self.last_known_stop_energy.is_none()
                        || self.last_known_stop_energy == Some(0)
                    {
                        info!(
                            "## session {session_id}: current {:.3} kWh (SoC can not be determined)",
                            energy as f64 / 1_000f64
                        );
                    } else {
                        let soc_progress = cs.add_snapshot(snapshot);
                        if soc_progress.is_complete() && !cs.is_complete() {
                            info!(">> Stopping session {session_id}: {soc_progress}");
                            cs.set_state(ChargingSessionState::SocCapReached);
                            self.call_queue.push_back(Action::RemoteStopTransaction(
                                call::RemoteStopTransaction {
                                    transaction_id: session_id,
                                },
                            ));
                        } else {
                            info!("## session {session_id}: {soc_progress}");
                        }
                    }
                }
            } else {
                warn!(
                    ">> MeterValues transaction id mismatch {transaction_id}, expected {session_id}",
                );
                // FIXME update transaction id in current session
            }
        } else if let Some(energy) = mv.active_energy_import {
            if let Some(transaction_id) = transaction_id {
                warn!(
                    "## adding missed charging session with transaction id {transaction_id}, \
                    last known stop energy: {:.3}",
                    self.last_known_stop_energy.unwrap_or_default() as f64 / 1_000f64,
                );
                self.check_bms_on_missed_session();

                self.charging_session = Some(ChargingSession::with_state(
                    self.bms.clone(),
                    ChargingSessionState::Unknown,
                    self.last_known_stop_energy.unwrap_or_default(),
                    transaction_id,
                    mv.timestamp,
                ));
            } else {
                // no known session in progress
                self.update_last_known_stop_energy(energy);
            }
        }
    }

    fn update_last_known_stop_energy(&mut self, meter_energy: u64) {
        match self.last_known_stop_energy {
            Some(last) if last == meter_energy => (),
            Some(ref mut last) if *last < meter_energy => {
                if self.charging_session.is_none() {
                    // only log when no charging session is in progress
                    info!(
                        "## updating last known stop energy: {:.3} kWh, \
                        previous: {:.3} kWh",
                        meter_energy as f64 / 1_000f64,
                        *last as f64 / 1_000f64
                    );
                }
                *last = meter_energy;
            }
            None => {
                info!(
                    "## setting last known stop energy: {:.3} kWh",
                    meter_energy as f64 / 1_000f64,
                );
                self.last_known_stop_energy = Some(meter_energy);
            }
            Some(last) => {
                info!(
                    "## ignoring last meter energy: {:.3} kWh, \
                    lower than last known stop energy: {:.3} kWh",
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
                    "## last stop energy is unknown => \
                    SoC & SoC cap will not have the expected effect",
                );
            } else {
                warn!("## ensure the SoC & SoC cap are set as expected");
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
        debug!(">> incoming {call_result:?}");

        match self
            .call_response_tracker
            .resolve(call_result)
            .context("resolving call result")
        {
            Ok(TypedCallResult::GetConfiguration(result)) => {
                info!(">> incoming {result:#?}");
            }
            Ok(TypedCallResult::ClearChargingProfile(result)) => {
                info!(">> incoming {result:?}");
            }
            Ok(TypedCallResult::SetChargingProfile(result)) => {
                if result.payload.status == v16::enums::ChargingProfileStatus::Accepted {
                    info!(">> charging profile accepted");
                } else {
                    error!(">> charging profile: {:?}", result.payload.status);
                    if let Some(mut schedule) = self.charging_schedule.take() {
                        schedule.inactivate();
                    }
                }
            }
            Ok(TypedCallResult::ChangeAvailability(result)) => {
                info!(">> incoming {result:#?}");
            }
            Ok(TypedCallResult::RemoteStartTransaction(result)) => {
                info!(">> incoming {result:#?}");
            }
            Ok(TypedCallResult::RemoteStopTransaction(result)) => {
                info!(">> incoming {result:#?}");
            }
            other => {
                info!(">> incoming {other:?}");
            }
        }
    }

    async fn send_action(&mut self, action: Action) -> anyhow::Result<()> {
        self.send_action_id += 1;
        info!("<< sending {} {action:?}", self.send_action_id);
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

                    if let Some(action) = self.call_queue.pop_front()
                        && let Err(err) = self.send_action(action).await
                    {
                        error!("failed to send first action: {err}");
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
