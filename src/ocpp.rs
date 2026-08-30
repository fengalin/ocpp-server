use anyhow::Context;
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

use crate::{ChargingSchedule, Evse, measurements::*, schedule};

const HEARTBEAT_INTERVAL_S: u32 = 3600;

#[derive(Debug)]
pub struct OcppInterface {
    send_action_id: usize,
    pending_response: Option<Message>,
    pending_commands: VecDeque<CommandToChargingPoint>,
    call_response_tracker: PendingCalls,
}

impl OcppInterface {
    pub fn new() -> Self {
        OcppInterface {
            send_action_id: 0,
            pending_response: None,
            pending_commands: VecDeque::new(),
            call_response_tracker: PendingCalls::new(),
        }
    }

    pub fn push_command(&mut self, command: CommandToChargingPoint) {
        self.pending_commands.push_back(command);
    }

    /// Handles the incoming message
    ///
    /// Returns `Some(reponse)` if applicable.
    pub fn handle_incoming_message(&mut self, evse: &mut Evse, msg: &str) -> Option<String> {
        // FIXME for every early return, we should enqueue an error Message

        let Ok(msg) = parse::deserialize_to_message(msg) else {
            error!("Failed to deserialize message: {msg}");
            return None;
        };
        match msg {
            Message::Call(call) => {
                if let Err(err) = self.handle_incoming_call(evse, call) {
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
            Message::CallResult(call_result) => self.handle_incoming_call_result(evse, call_result),
            Message::CallError(err) => error!("{err:?}"),
        }

        None
    }

    fn handle_incoming_call(&mut self, evse: &mut Evse, call: Call) -> anyhow::Result<()> {
        trace!(">> incoming {call:?}");

        match call.payload {
            Action::StatusNotification(status) => {
                use chrono::{Datelike, Timelike, Utc, Weekday};

                let action_ts = status.timestamp.map_or_else(Utc::now, |ts| ts.inner());
                if matches!(status.status, ChargePointStatus::Charging)
                    && action_ts.weekday() == Weekday::Mon
                    && action_ts.hour() == 0
                    && action_ts.minute() == 0
                {
                    // The charging point I use clears the charging profile
                    // on Monday 0:00 UTC (it seems to set a charging profile at some
                    // later point, but that's still unclear when nor what profile).
                    // If the EV is connected, charging starts regardless of any user
                    // defined charging plan & SoC is only capted if specified.
                    // As a workaround, push current charging schedule now
                    warn!(
                        "alleged charging schedule removal by charging point: \
                            reapplying charging schedule"
                    );
                    evse.refresh_charging_schedule();
                }

                evse.have_charging_point_status(&status);
                self.prepare_response(status, call.unique_id, call_result::EmptyResponse {});
            }
            Action::StartTransaction(start) => {
                let transaction_id = evse.have_start_transaction(&start);
                self.prepare_response(
                    start,
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
            Action::StopTransaction(stop) => {
                evse.have_stop_transaction(&stop);
                self.prepare_response(
                    stop,
                    call.unique_id,
                    call_result::StopTransaction { id_tag_info: None },
                );
            }
            Action::MeterValues(action_mv) => {
                let mut meter_val_selection: Vec<_> = action_mv
                    .meter_value
                    .iter()
                    .map(|mv| MeterValueSelection::new(mv, action_mv.transaction_id))
                    .collect();

                // FIXME check if we need to handle possibly multiple items
                if meter_val_selection.len() > 1 {
                    info!(">> MeterValues {meter_val_selection:?}");
                }

                if let Some(mv) = meter_val_selection.pop() {
                    evse.have_charging_point_meter_values(mv);
                }

                self.prepare_response(action_mv, call.unique_id, call_result::EmptyResponse {});
            }
            Action::DataTransfer(action_dt) => {
                for d in action_dt.data.iter() {
                    let Ok(mut dpm_data_set) = serde_json::from_str::<Vec<Dpm>>(d) else {
                        error!(">> failed to parse DPM data set");
                        continue;
                    };

                    // FIXME check if we need to handle possibly multiple items
                    if dpm_data_set.len() > 1 {
                        info!(">> DPM data set {dpm_data_set:?}");
                    }

                    if let Some(dpm_data) = dpm_data_set.pop() {
                        evse.have_dpm_data(DpmSelection::from(dpm_data));
                    }
                }

                self.prepare_response(
                    action_dt,
                    call.unique_id,
                    call_result::DataTransfer {
                        status: DataTransferStatus::Accepted,
                        data: None,
                    },
                );
            }
            Action::Heartbeat(heartbeat) => {
                info!(">> incoming {heartbeat:?}");
                self.prepare_response(
                    heartbeat,
                    call.unique_id,
                    call_result::Heartbeat {
                        current_time: DateTimeWrapper::new(chrono::Utc::now()),
                    },
                );
            }
            Action::BootNotification(boot_notif) => {
                info!(">> incoming {boot_notif:?}");
                self.prepare_response(
                    boot_notif,
                    call.unique_id,
                    call_result::BootNotification {
                        current_time: DateTimeWrapper::new(chrono::Utc::now()),
                        interval: HEARTBEAT_INTERVAL_S as _,
                        status: RegistrationStatus::Accepted,
                    },
                );
            }
            Action::SecurityEventNotification(sec_evt_notif) => {
                if sec_evt_notif.event_type == "SettingSystemTime" {
                    trace!(">> incoming {sec_evt_notif:?}");
                } else {
                    info!(">> incoming {sec_evt_notif:?}");
                }
                self.prepare_response(sec_evt_notif, call.unique_id, call_result::EmptyResponse {});
            }
            Action::Authorize(authorize) => {
                // FIXME is this even sent with this charging point?
                info!(">> incoming {authorize:?}");
                self.prepare_response(
                    authorize,
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

    fn handle_incoming_call_result(&mut self, evse: &mut Evse, call_result: CallResultRaw) {
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
                }
            }
            Ok(TypedCallResult::SetChargingProfile(result)) => {
                if result.payload.status == v16::enums::ChargingProfileStatus::Accepted {
                    info!(">> charging profile accepted");
                } else {
                    error!(">> charging profile: {:?}", result.payload.status);
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
                evse.have_session_stopped_status(
                    result.payload.status == RemoteStartStopStatus::Accepted,
                );
            }
            other => {
                info!(">> incoming {other:?}");
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

    pub fn pending_calls(&mut self, evse: &mut Evse) -> Vec<String> {
        let mut res = vec![];

        for command in self
            .pending_commands
            .pop_front()
            .into_iter()
            .chain(evse.pop_command())
        {
            use CommandToChargingPoint::*;
            let actions = match command {
                Reboot => {
                    info!("<< sending Reset Soft");
                    evse.permanent_0w_set();
                    // After a reset, the EVSE starts the last chaging plan that
                    // was set, regardless of the moment it was supposed to start.
                    // Set a 0 W permanent limit to make sure we don't start
                    // charging unexpectedly.
                    vec![
                        Action::ClearChargingProfile(call::ClearChargingProfile {
                            id: None,
                            connector_id: None,
                            charging_profile_purpose: None,
                            stack_level: None,
                        }),
                        Action::SetChargingProfile(
                            schedule::SetChargingProfile::builder(
                                schedule::ChargingSchedule::new(),
                            )
                            .build(),
                        ),
                        Action::Reset(call::Reset {
                            reset_type: ResetType::Soft,
                        }),
                    ]
                }
                SetServerAddress(address) => {
                    info!("<< setting server address to {address}");
                    evse.permanent_0w_set();
                    // see comment about reboot above
                    vec![
                        Action::ChangeConfiguration(call::ChangeConfiguration {
                            key: "CS_URL".to_string(),
                            value: address,
                        }),
                        Action::SetChargingProfile(
                            schedule::SetChargingProfile::builder(
                                schedule::ChargingSchedule::new(),
                            )
                            .build(),
                        ),
                        Action::Reset(call::Reset {
                            reset_type: ResetType::Soft,
                        }),
                    ]
                }
                SetChargingSchedule(schedule) => {
                    info!("<< sending SetChargingProfile");
                    vec![
                        Action::ClearChargingProfile(call::ClearChargingProfile {
                            id: None,
                            connector_id: None,
                            charging_profile_purpose: None,
                            stack_level: None,
                        }),
                        Action::SetChargingProfile(
                            schedule::SetChargingProfile::builder(schedule).build(),
                        ),
                    ]
                }
                StopTransaction(transaction_id) => {
                    info!("<< sending StopTransaction {transaction_id}");
                    vec![Action::RemoteStopTransaction(call::RemoteStopTransaction {
                        transaction_id,
                    })]
                }
            };

            for action in actions.into_iter() {
                self.send_action_id += 1;
                let call = Call::new(format!("{}.occp-server-test", self.send_action_id), action);
                match self
                    .call_response_tracker
                    .send_call(call)
                    .context("send_call")
                {
                    Ok(call) => res.push(call),
                    Err(err) => {
                        error!("skipping call due to error: {err}");
                    }
                }
            }
        }

        res
    }
}

#[derive(Debug)]
pub enum CommandToChargingPoint {
    Reboot,
    SetChargingSchedule(ChargingSchedule),
    SetServerAddress(String),
    StopTransaction(i32),
}
