use anyhow::Context;
use chrono::{Datelike, Timelike, Utc};
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

use crate::{
    Bms, ChargingPlan, ChargingSchedule, ChargingSession, ChargingSessionSnapshot,
    ChargingSessionState, Database, SoC, args, bms::SoCProgress, measurements::*, schedule,
};

const HEARTBEAT_INTERVAL_S: u32 = 3600;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
enum EnergyTracker {
    Increasing(u64),
    Probation(u64),
    Stationnary(u64),
    #[default]
    Unknown,
}

impl EnergyTracker {
    fn have_energy(&mut self, energy: u64) {
        use EnergyTracker::*;
        *self = match self {
            Increasing(v) if *v < energy => Increasing(energy),
            // FIXME add an increasing to stationnary probation
            // state before going back to stationnary?
            Increasing(_) => Stationnary(energy),
            Probation(v) if *v < energy => Increasing(energy),
            Probation(_) => Stationnary(energy),
            Stationnary(v) if *v < energy => Increasing(energy),
            Stationnary(_) => Stationnary(energy),
            Unknown => Probation(energy),
        };
    }
}

#[derive(Debug)]
pub struct Evse {
    connector_id: Option<u32>,
    bms: Bms,
    last_known_tid: i32,
    energy_tracker: EnergyTracker,
    last_known_stop_energy: Option<u64>,
    meter_value_observer: MeterValueObserver,
    last_dpm: Option<DpmSelection>,
    send_action_id: usize,
    pending_response: Option<Message>,
    call_response_tracker: PendingCalls,
    call_queue: VecDeque<Action>,
    charging_session: Option<ChargingSession>,
    charging_schedule: Option<ChargingSchedule>,
}

impl Evse {
    pub fn new(
        bms: Bms,
        charging_plan: Option<ChargingPlan>,
        last_charging_session: Option<ChargingSession>,
        mut last_charging_schedule: Option<ChargingSchedule>,
        command: args::Command,
    ) -> Self {
        let mut this = Evse {
            connector_id: None,
            bms,
            last_known_tid: 0,
            last_known_stop_energy: None,
            energy_tracker: Default::default(),
            meter_value_observer: Default::default(),
            last_dpm: None,
            send_action_id: 0,
            pending_response: None,
            call_response_tracker: PendingCalls::new(),
            call_queue: VecDeque::new(),
            charging_session: None,
            charging_schedule: None,
        };

        if let Some(ref cs) = last_charging_session {
            this.last_known_tid = cs.transaction_id();

            if cs.is_complete() {
                this.last_known_stop_energy = Some(cs.last_energy());
            } else {
                let first_snapshot = cs.first_snapshot().expect("at least one");
                this.last_known_stop_energy = Some(first_snapshot.energy);

                this.charging_session = last_charging_session;
            }
        }

        if let Some(schedule) = last_charging_schedule.take()
            && schedule.is_active()
        {
            this.charging_schedule = Some(schedule);
        }

        use args::Command::*;
        match command {
            Run => {
                if let Some(charging_plan) = charging_plan {
                    info!("using charging plan {charging_plan:?}");

                    if let Some(charging_schedule) = charging_plan.to_charging_schedule(&this.bms) {
                        this.push_charging_schedule(charging_schedule);
                    }
                }
            }
            StopSession => {
                let cs = this.charging_session.as_ref().expect("checked by caller");
                this.call_queue.push_back(Action::RemoteStopTransaction(
                    call::RemoteStopTransaction {
                        transaction_id: cs.transaction_id(),
                    },
                ));
            }
            Reboot => {
                // After a reset, the EVSE starts the last chaging plan that
                // was set, regardless of the moment it was supposed to start.
                // Set a 0 W permanent limit to make sure we don't start
                // charging unexpectedly.
                this.push_charging_schedule(ChargingSchedule::new());

                this.call_queue.push_back(Action::Reset(call::Reset {
                    reset_type: ResetType::Soft,
                }));
            }
            SetServerIp(ip_address) => {
                let server_ip = ip_address.get_ip_address().expect("checked by caller");
                this.call_queue
                    .push_back(Action::ChangeConfiguration(call::ChangeConfiguration {
                        key: "CS_URL".to_string(),
                        value: format!("ws://{server_ip}:9000"),
                    }));
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

    /// Handles the incoming message
    ///
    /// Returns `Some(reponse)` if applicable.
    pub fn handle_incoming_message(&mut self, msg: &str) -> Option<String> {
        // FIXME for every early return, we should enqueue an error Message

        let Ok(msg) = parse::deserialize_to_message(msg) else {
            error!("Failed to deserialize message: {msg}");
            return None;
        };
        match msg {
            Message::Call(call) => {
                if let Err(err) = self.handle_incoming_call(call) {
                    error!("error handling incoming call: {err}");
                    return None;
                }

                if let Some(pending_response) = self.pending_response.take() {
                    trace!("<< sending response {pending_response:?}");
                    match parse::serialize_message(&pending_response) {
                        Ok(response) => return Some(response),
                        Err(err) => {
                            error!("Failed to serialize response: {err}, {pending_response:?}");
                        }
                    }
                }
            }
            Message::CallResult(call_result) => self.handle_incoming_call_result(call_result),
            Message::CallError(err) => error!("{err:?}"),
        }

        None
    }

    fn handle_incoming_call(&mut self, call: Call) -> anyhow::Result<()> {
        trace!(">> incoming {call:?}");

        match call.payload {
            Action::StatusNotification(action) => {
                if action.connector_id != 0 {
                    let connector_log = format!(
                        ">> connector {}: {:?}{}",
                        action.connector_id,
                        action.status,
                        if let Some(ts) = action.timestamp {
                            format!(", timestamp: {}", ts.inner().with_timezone(&chrono::Local))
                        } else {
                            "".to_string()
                        },
                    );

                    if !matches!(action.error_code, ChargePointErrorCode::NoError) {
                        warn!("{connector_log}, {:?}", action.error_code);
                    } else {
                        let connector_ok_log = format!(
                            "{connector_log}{}",
                            if let Some(ref info) = action.info
                                && !info.is_empty()
                            {
                                format!(", info: {info}")
                            } else {
                                String::new()
                            },
                        );

                        let action_ts = action.timestamp.map_or_else(Utc::now, |ts| ts.inner());
                        if matches!(action.status, ChargePointStatus::Charging) {
                            warn!("{connector_ok_log}");

                            if action_ts.weekday() == chrono::Weekday::Mon
                                && action_ts.hour() == 0
                                && action_ts.minute() == 0
                                && let Some(mut cs) = self.charging_schedule.take()
                            {
                                // The charging point I use clears the charging profile
                                // on Monday 0:00 UTC (it seems to set a charging profile at some
                                // later point, but that's still unclear when nor what profile).
                                // If the EV is connected, charging starts regardless of any use
                                // defined charging plan & SoC is only capted if specified.
                                // As a workaround, push current charging schedule now
                                warn!(
                                    "alleged charging schedule removal by charging point: \
                                    resetting charging schedule"
                                );
                                cs.reset_set_time();
                                self.push_charging_schedule(cs);
                            }
                        } else {
                            info!("{connector_ok_log}");
                        }

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
                                        action_ts,
                                        cs.last_energy(),
                                        ChargingSessionState::Error(
                                            "Got connector available while session was still active"
                                                .to_string(),
                                        ),
                                    );
                                    self.log_session_progress();
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
                                    self.log_session_progress();
                                }
                            }
                            ChargePointStatus::SuspendedEV | ChargePointStatus::Faulted => {
                                // FIXME reached 100% => not restarting the session?
                                if let Some(mut cs) = self.charging_session.take() {
                                    cs.set_state(action.status.clone());
                                    self.log_session_progress();
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
                if action.event_type == "SettingSystemTime" {
                    trace!(">> incoming {action:?}");
                } else {
                    info!(">> incoming {action:?}");
                }
                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::MeterValues(action) => {
                let mut meter_val_selection: Vec<_> = action
                    .meter_value
                    .iter()
                    .map(|mv| MeterValueSelection::new(mv, action.transaction_id))
                    .collect();

                // FIXME check if we need to handle possibly multiple items
                if meter_val_selection.len() > 1 {
                    info!(">> MeterValues {meter_val_selection:?}");
                }

                if let Some(mut mv) = meter_val_selection.pop() {
                    self.meter_value_observer.consolidate(&mut mv);

                    if self.meter_value_observer.pertinent_to_user(&mv) {
                        info!(">> MeterValues {mv}");
                    } else {
                        trace!(">> MeterValues {mv:?}");
                    }

                    self.handle_meter_value(mv);
                }

                self.prepare_response(action, call.unique_id, call_result::EmptyResponse {});
            }
            Action::DataTransfer(action) => {
                for d in action.data.iter() {
                    let Ok(mut dpm_data_set) = serde_json::from_str::<Vec<Dpm>>(d) else {
                        error!(">> failed to parse DPM data set");
                        continue;
                    };

                    // FIXME check if we need to handle possibly multiple items
                    if dpm_data_set.len() > 1 {
                        info!(">> DPM data set {dpm_data_set:?}");
                    }

                    if let Some(dpm_data) = dpm_data_set.pop() {
                        let dpm_data = DpmSelection::from(dpm_data);
                        if dpm_data.active_power_import.is_some()
                            && self
                                .last_dpm
                                .as_ref()
                                .is_some_and(|last_dpm| *last_dpm != dpm_data)
                            || self.last_dpm.is_none()
                        {
                            info!(">> DPM data {dpm_data}");
                            self.last_dpm = Some(dpm_data);
                        } else {
                            trace!(">> DPM data {dpm_data:?}");
                        }
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
                        "## new session start ending previous session with id: {}, tid {}, state: {}",
                        cs.session_id(),
                        cs.transaction_id(),
                        cs.state(),
                    );
                    cs.stop(
                        Utc::now(),
                        cs.last_energy(),
                        ChargingSessionState::Error(
                            "Got start transaction while session was still active".to_string(),
                        ),
                    );
                }

                // We can't block a start transaction due to SoC cap being reached
                // as the charging point attempts to restart in a loop.
                // That's not really a problem though: if a new transaction
                // starts and the server previously stopped a transaction,
                // it means either:
                // * the EV was unplugged / plugged again => it's up to the
                //   user to configure the server as needed.
                // * the charging point was rebooted, in which case an
                //   permanent 0 W charging plan is applied.
                // * TODO check what happens after a reboot due to
                //   main power interuption.
                let transaction_id = self.get_next_transaction_id();
                let cs = ChargingSession::new(
                    self.bms.clone(),
                    action.timestamp.inner(),
                    transaction_id,
                    action.meter_start,
                );
                info!(
                    "## starting transaction with id: {transaction_id}, timestamp: {}, \
                    meter start: {:.3} kWh",
                    action.timestamp.inner().with_timezone(&chrono::Local),
                    action.meter_start as f64 / 1_000.0,
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
                    let cur_transaction_id = cs.transaction_id();
                    if cur_transaction_id == action.transaction_id {
                        info!(
                            "## transaction with id: {} stopped, timestamp: {}, \
                            meter stop: {:.3} kWh, reason: {:?}",
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop as f64 / 1_000.0,
                            action.reason
                        );
                        cs.stop(action.timestamp.inner(), action.meter_stop, action.reason)
                    } else {
                        warn!(
                            "## transaction with id: {} stopped (expected {cur_transaction_id}), \
                            timestamp: {}, meter stop: {:.3} kWh, reason: {:?}",
                            action.transaction_id,
                            action.timestamp.inner().with_timezone(&chrono::Local),
                            action.meter_stop as f64 / 1_000.0,
                            action.reason
                        );
                        self.have_transaction_id(action.transaction_id);
                        cs.stop(
                            Utc::now(),
                            cs.last_energy(),
                            ChargingSessionState::Error(
                                "Got stop transaction for another session".to_string(),
                            ),
                        );
                    }
                } else {
                    warn!(
                        "## transaction with id: {} stopped (unexpected), \
                        timestamp: {}, meter stop: {:.3} kWh, reason: {:?}",
                        action.transaction_id,
                        action.timestamp.inner().with_timezone(&chrono::Local),
                        action.meter_stop as f64 / 1_000.0,
                        action.reason
                    );
                    self.have_transaction_id(action.transaction_id);
                    ChargingSession::save_missing_stopped_session(
                        Some(action.timestamp.inner()),
                        action.reason,
                        action.meter_stop,
                        action.transaction_id,
                    );
                }

                self.energy_tracker.have_energy(action.meter_stop);
                self.last_known_stop_energy = Some(action.meter_stop);

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

        Ok(())
    }

    fn handle_meter_value(&mut self, mv: MeterValueSelection) {
        let Some(energy) = mv.active_energy_import else {
            return;
        };

        use EnergyTracker::*;
        self.energy_tracker.have_energy(energy);
        match self.energy_tracker {
            Increasing(_) => {
                let Some(transaction_id) = mv.transaction_id else {
                    debug!(
                        "## MeterValue without transaction id, but incresing energy, \
                        waiting for next MeterValue"
                    );
                    return;
                };

                match self.charging_session.as_mut() {
                    Some(cs) if cs.transaction_id() == transaction_id => {
                        let snapshot = ChargingSessionSnapshot::builder(mv.timestamp, energy)
                            .power(mv.active_power_import)
                            .l1_voltage(mv.voltage_l1)
                            .temperature(mv.temperature)
                            .build();

                        let outstg_sched = self.charging_schedule.as_ref().map(|s| {
                            s.outstanding(
                                chrono::Local::now().naive_local(),
                                self.bms.constant_power_loss,
                            )
                        });
                        let soc_progress = cs.add_snapshot(snapshot);
                        if soc_progress.is_complete()
                            && !cs.is_complete()
                            // don't stop if we are nearly done with the schedule
                            // (less than 1% here) so we can start a new one without unplugging
                            // FIXME could be an option
                            // FIXME implement an optional intermediate SoC target for multi-period scheds
                            && outstg_sched.is_none_or(|outstg| outstg.energy > self.bms.capacity / 100.0)
                        {
                            info!("## Stopping session {transaction_id}: {soc_progress}");
                            cs.set_state(ChargingSessionState::SoCCapReached);
                            self.call_queue.push_back(Action::RemoteStopTransaction(
                                call::RemoteStopTransaction { transaction_id },
                            ));
                        }
                        self.log_session_progress();

                        return;
                    }
                    Some(cs) => {
                        warn!(
                            "## MeterValues transaction id mismatch {transaction_id}, \
                            expected {}",
                            cs.transaction_id()
                        );
                        cs.set_state(ChargingSessionState::Error(
                            "transaction id mismatch".to_string(),
                        ));
                        self.log_session_progress();
                        self.charging_session = None;
                    }
                    None => {
                        warn!(
                            "## MeterValues with transaction id and increasing energy \
                            for unknown session"
                        );
                    }
                }

                let mut bms = self.bms.clone();
                let start_energy = if let Some(last_stop_energy) = self.last_known_stop_energy {
                    last_stop_energy
                } else {
                    warn!(
                        "## initial energy can not be determined, check the configured SoC, \
                        SoC cap and charging schedule"
                    );
                    bms.initial_soc.update(SoC::Unknown);
                    bms.current_soc.update(SoC::Unknown);
                    energy
                };

                warn!(
                    "## adding new charging session for {transaction_id}, \
                    increasing start energy: {start_energy}"
                );
                self.have_transaction_id(transaction_id);
                self.charging_session = Some(ChargingSession::with_state(
                    bms,
                    transaction_id,
                    ChargingSessionState::Preparing,
                    energy,
                    mv.timestamp,
                ));
            }
            Stationnary(_) => {
                let Some(transaction_id) = mv.transaction_id else {
                    debug!(
                        "## MeterValue without transaction id, stationary \
                        waiting for next MeterValue"
                    );
                    return;
                };

                match self.charging_session.as_mut() {
                    Some(cs) if cs.transaction_id() == transaction_id => {
                        return;
                    }
                    Some(cs) => {
                        warn!(
                            "## MeterValues transaction id mismatch {transaction_id}, \
                            expected {}",
                            cs.transaction_id()
                        );
                        cs.set_state(ChargingSessionState::Error(
                            "transaction id mismatch".to_string(),
                        ));
                        self.log_session_progress();
                    }
                    None => {
                        info!(
                            ">> MeterValues with stationnary energy \
                            for unknown session: {transaction_id}"
                        );
                    }
                }

                self.last_known_stop_energy = Some(energy);

                warn!(
                    "## adding new charging session for {transaction_id}, \
                    stationnary start energy: {:.3} kWh",
                    energy as f64 / 1_000.0
                );
                self.have_transaction_id(transaction_id);
                self.charging_session = Some(ChargingSession::with_state(
                    self.bms.clone(),
                    transaction_id,
                    ChargingSessionState::Preparing,
                    energy,
                    mv.timestamp,
                ));
            }
            Probation(_) => info!(">> MeterValue with energy set to probation"),
            Unknown => unreachable!("energy added"),
        }
    }

    fn log_session_progress(&self) {
        let Some(ref cs) = self.charging_session else {
            return;
        };
        info!(
            "## session {cs} / {}{}",
            SoCProgress::from_soc_and_cap(cs.last_soc(), self.bms.soc_cap).cap(),
            if let Some(outstg_sched) = self.charging_schedule.as_ref().map(|s| s.outstanding(
                chrono::Local::now().naive_local(),
                self.bms.constant_power_loss,
            )) {
                format!(", outstanding: {outstg_sched}")
            } else {
                "".to_string()
            }
        );
    }

    fn have_transaction_id(&mut self, transaction_id: i32) {
        if self.last_known_tid < transaction_id {
            self.last_known_tid = transaction_id;
        }
    }

    fn get_next_transaction_id(&mut self) -> i32 {
        self.last_known_tid += 1;
        self.last_known_tid
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
                if result.payload.status == v16::enums::ClearChargingProfileStatus::Accepted {
                    info!(">> clear charging profile accepted");
                } else {
                    error!(">> clear charging profile: {:?}", result.payload.status);
                    if let Some(mut schedule) = self.charging_schedule.take() {
                        schedule.inactivate();
                    }
                }
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
            Ok(TypedCallResult::Reset(result)) => {
                if result.payload.status == v16::enums::ResetStatus::Accepted {
                    info!(">> reset accepted");
                } else {
                    error!(">> reset rejected");
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

    pub fn pop_call(&mut self) -> Option<String> {
        // while no error
        while let Some(action) = self.call_queue.pop_front() {
            self.send_action_id += 1;
            match &action {
                Action::ClearChargingProfile(_) => {
                    info!("<< sending {} ClearChargingProfile", self.send_action_id);
                }
                Action::SetChargingProfile(_) => {
                    info!("<< sending {} SetChargingProfile", self.send_action_id);
                }
                other => {
                    info!("<< sending {} {other:?}", self.send_action_id);
                }
            }
            let call = Call::new(format!("{}.occp-server-test", self.send_action_id), action);
            match self
                .call_response_tracker
                .send_call(call)
                .context("send_call")
            {
                Ok(call) => return Some(call),
                Err(err) => error!("skipping call due to error: {err}"),
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_tracker() {
        use EnergyTracker::*;

        // probation to stationnary
        let mut tracker = EnergyTracker::default();
        assert_eq!(tracker, Unknown);

        let mut energy = 100;
        tracker.have_energy(energy);
        assert_eq!(tracker, Probation(energy));

        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));
        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        // probation to increasing
        let mut tracker = EnergyTracker::default();
        assert_eq!(tracker, Unknown);

        let mut energy = 100;
        tracker.have_energy(energy);
        assert_eq!(tracker, Probation(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));
    }
}
