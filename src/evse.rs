use chrono::Utc;
use log::*;
use ocpp_rs::v16::{call, enums::*};
use tokio::sync::broadcast;

use std::collections::VecDeque;

use crate::{
    Bms, ChargingPlan, ChargingSchedule, ChargingSession, ChargingSessionSnapshot,
    ChargingSessionState, CommandToChargingPoint, Database, SoC,
    bms::SoCProgress,
    measurements::*,
    notification::{self, ChargePointNotification},
};

#[derive(Debug)]
pub struct Evse {
    bms: Bms,
    last_known_tid: i32,
    energy_tracker: EnergyTracker,
    last_known_stop_energy: Option<u64>,
    meter_value_observer: MeterValueObserver,
    last_dpm: Option<DpmSelection>,
    command_queue: VecDeque<CommandToChargingPoint>,
    charging_session: Option<ChargingSession>,
    charging_schedule: Option<ChargingSchedule>,
    notif_tx: broadcast::Sender<ChargePointNotification>,
}

impl Evse {
    pub fn new(bms: Bms, notif_tx: broadcast::Sender<ChargePointNotification>) -> Self {
        let (last_charging_session, mut last_charging_schedule) = {
            let db = Database::get();
            (
                db.get_last_charging_session(&bms)
                    .expect("valid db interface"),
                db.get_active_charging_schedule()
                    .expect("valid db interface"),
            )
        };

        let mut this = Evse {
            bms,
            last_known_tid: 0,
            last_known_stop_energy: None,
            energy_tracker: Default::default(),
            meter_value_observer: Default::default(),
            last_dpm: None,
            command_queue: VecDeque::new(),
            charging_session: None,
            charging_schedule: None,
            notif_tx,
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

        this
    }

    pub fn set_charging_plan(&mut self, charging_plan: ChargingPlan) {
        if let Some(mut prev_schedule) = self.charging_schedule.take() {
            prev_schedule.inactivate();
        };

        let Some(mut schedule) = charging_plan.to_charging_schedule(&self.bms) else {
            warn!("no charging plans");
            return;
        };

        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule.clone());

        self.command_queue
            .push_back(CommandToChargingPoint::SetChargingSchedule(schedule));
    }

    pub fn refresh_charging_schedule(&mut self) {
        let Some(mut schedule) = self.charging_schedule.take() else {
            return;
        };

        info!("## refreshing charging schedule");
        schedule.inactivate();

        schedule.reset_set_time();
        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule.clone());

        self.command_queue
            .push_back(CommandToChargingPoint::SetChargingSchedule(schedule));
    }

    pub fn stop_current_session(&mut self) {
        let Some(cs) = self.charging_session.as_ref() else {
            return;
        };
        info!(
            "## stopping transaction with id: {} as per user request",
            cs.transaction_id(),
        );
        self.command_queue
            .push_back(CommandToChargingPoint::StopTransaction(cs.transaction_id()));
    }

    pub fn have_session_stopped_status(&mut self, accepted: bool) {
        let Some(mut cs) = self.charging_session.take() else {
            return;
        };

        if !accepted {
            error!(
                "## transaction with id: {} NOT stopped",
                cs.transaction_id()
            );
            return;
        }

        let energy = self.energy_tracker.current();
        info!(
            "## transaction with id: {} stopped, last known energy: {:.3} kWh",
            cs.transaction_id(),
            energy.map_or(f64::NAN, |e| e as f64 / 1_000.0),
        );
        cs.stop(
            Utc::now(),
            energy.unwrap_or_default(),
            ChargingSessionState::StoppedByUser,
        );

        if let Some(energy) = energy {
            self.last_known_stop_energy = Some(energy);
        }
    }

    pub fn have_charging_point_status(&mut self, status: &call::StatusNotification) {
        if status.connector_id != 0 {
            let connector_log = format!(
                ">> connector {}: {:?}{}",
                status.connector_id,
                status.status,
                if let Some(ts) = status.timestamp {
                    format!(", timestamp: {}", ts.inner().with_timezone(&chrono::Local))
                } else {
                    "".to_string()
                },
            );

            if !matches!(status.error_code, ChargePointErrorCode::NoError) {
                warn!("{connector_log}, {:?}", status.error_code);
            } else {
                let connector_ok_log = format!(
                    "{connector_log}{}",
                    if let Some(ref info) = status.info
                        && !info.is_empty()
                    {
                        format!(", info: {info}")
                    } else {
                        String::new()
                    },
                );

                let status_ts = status.timestamp.map_or_else(Utc::now, |ts| ts.inner());
                if matches!(status.status, ChargePointStatus::Charging) {
                    warn!("{connector_ok_log}");
                } else {
                    info!("{connector_ok_log}");
                }

                match status.status {
                    ChargePointStatus::Available => {
                        if let Some(ref mut cs) = self.charging_session
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
                                status.status,
                            );
                            cs.stop(
                                status_ts,
                                cs.last_energy(),
                                ChargingSessionState::Error(
                                    "Got connector available while session was still active"
                                        .to_string(),
                                ),
                            );
                            self.notif_session_progress();
                            self.charging_session = None;
                        } else {
                            self.notif_state(notification::ChargeState::Available);
                        }
                    }
                    ChargePointStatus::Finishing => {
                        if let Some(ref mut cs) = self.charging_session {
                            if !cs.is_complete() {
                                // when the transaction was stopped by the server
                                // (SoC cap was reached), the status is Finishing.
                                // The only way to get it back to a status which
                                // would allow starting a new session is by unplugging
                                // the EV first or by rebooting the charging point.
                                // Finishing can also happen when the EV is unplugged
                                warn!(
                                    "## ending active session with id: {} \
                                due to connector status: {:?}",
                                    cs.session_id(),
                                    status.status,
                                );
                                cs.stop(status_ts, cs.last_energy(), ChargePointStatus::Finishing);
                                self.notif_session_progress();
                            } else {
                                cs.set_state(status.status.clone());
                                self.notif_session_progress();
                            }
                        } else {
                            self.notif_state(notification::ChargeState::Finishing);
                        }
                    }
                    ChargePointStatus::Preparing
                    | ChargePointStatus::Charging
                    | ChargePointStatus::SuspendedEVSE => {
                        // the EV is not charging due to EVSE not providing
                        // energy (e.g. charging period with power limit set to 0)
                        // however, the session can still be restarted
                        if let Some(ref mut cs) = self.charging_session {
                            // we can't ensure this is the same session
                            // and can only hope we got at least one MeterValue
                            // with the transaction id before getting this
                            // StatusNotification
                            cs.set_state(status.status.clone());
                            self.notif_session_progress();
                        } else if matches!(status.status, ChargePointStatus::Preparing) {
                            self.notif_state(notification::ChargeState::Preparing);
                        } else {
                            // FIXME start a new session instead
                            self.notif_state(notification::ChargeState::UnknownSession);
                        }
                    }
                    ChargePointStatus::SuspendedEV => {
                        // FIXME reached 100% => not restarting the session?
                        if let Some(ref mut cs) = self.charging_session {
                            cs.set_state(status.status.clone());
                            self.notif_session_progress();
                        } else {
                            self.notif_state(notification::ChargeState::SuspendedEv);
                        }
                    }
                    ChargePointStatus::Faulted => {
                        // FIXME reached 100% => not restarting the session?
                        if let Some(ref mut cs) = self.charging_session {
                            cs.set_state(status.status.clone());
                            self.notif_session_progress();
                        } else {
                            self.notif_state(notification::ChargeState::Error);
                        }
                    }
                    _ => {
                        warn!("unhandled charging point status: {:?}", status.status);
                        self.notif_state(notification::ChargeState::Error);
                    }
                }
            }
        }
    }

    /// Handles incoming start transaction from charging point
    ///
    /// Returns the assigned transaction id
    pub fn have_start_transaction(&mut self, start: &call::StartTransaction) -> i32 {
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
            start.timestamp.inner(),
            transaction_id,
            start.meter_start,
        );
        info!(
            "## starting transaction with id: {transaction_id}, timestamp: {}, \
            meter start: {:.3} kWh",
            start.timestamp.inner().with_timezone(&chrono::Local),
            start.meter_start as f64 / 1_000.0,
        );
        self.charging_session = Some(cs);

        transaction_id
    }

    /// Handles incoming stop transaction from charging point
    pub fn have_stop_transaction(&mut self, stop: &call::StopTransaction) {
        if let Some(mut cs) = self.charging_session.take() {
            let cur_transaction_id = cs.transaction_id();
            if cur_transaction_id == stop.transaction_id {
                info!(
                    "## transaction with id: {} stopped, timestamp: {}, \
                    meter stop: {:.3} kWh, reason: {:?}",
                    stop.transaction_id,
                    stop.timestamp.inner().with_timezone(&chrono::Local),
                    stop.meter_stop as f64 / 1_000.0,
                    stop.reason
                );
                cs.stop(
                    stop.timestamp.inner(),
                    stop.meter_stop,
                    stop.reason.as_ref(),
                )
            } else {
                warn!(
                    "## transaction with id: {} stopped (expected {cur_transaction_id}), \
                    timestamp: {}, meter stop: {:.3} kWh, reason: {:?}",
                    stop.transaction_id,
                    stop.timestamp.inner().with_timezone(&chrono::Local),
                    stop.meter_stop as f64 / 1_000.0,
                    stop.reason
                );
                self.have_transaction_id(stop.transaction_id);
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
                stop.transaction_id,
                stop.timestamp.inner().with_timezone(&chrono::Local),
                stop.meter_stop as f64 / 1_000.0,
                stop.reason
            );
            self.have_transaction_id(stop.transaction_id);
            ChargingSession::save_missing_stopped_session(
                Some(stop.timestamp.inner()),
                stop.reason.as_ref(),
                stop.meter_stop,
                stop.transaction_id,
            );
        }

        self.energy_tracker.have_energy(stop.meter_stop);
        self.last_known_stop_energy = Some(stop.meter_stop);
    }

    pub fn have_charging_point_meter_values(&mut self, mut mv: MeterValueSelection) {
        self.meter_value_observer.consolidate(&mut mv);

        if self.meter_value_observer.pertinent_to_user(&mv) {
            info!(">> MeterValues {mv}");
        } else {
            trace!(">> MeterValues {mv:?}");
        }

        let Some(energy) = mv.active_energy_import else {
            return;
        };

        use EnergyTracker::*;
        self.energy_tracker.have_energy(energy);
        match self.energy_tracker {
            Increasing(_) => {
                let Some(transaction_id) = mv.transaction_id else {
                    debug!(
                        "## MeterValue without transaction id & incresing energy, \
                        waiting for next MeterValue"
                    );
                    return;
                };

                match self.charging_session.as_mut() {
                    Some(cs) if cs.transaction_id() == transaction_id => {
                        self.update_current_session(mv, energy);
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
                        self.notif_session_progress();
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
                        "## MeterValue without transaction id & stationary \
                        waiting for next MeterValue"
                    );
                    return;
                };

                match self.charging_session.as_mut() {
                    Some(cs) if cs.transaction_id() == transaction_id => {
                        if let Some(last_snapshot) = cs.last_snapshot()
                            && last_snapshot.energy != energy
                        {
                            // energy meter is stationnary but we are not up to date
                            self.update_current_session(mv, energy);
                        }
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
                        self.notif_session_progress();
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
            Probation(_) => {
                if let Some(ref cs) = self.charging_session
                    && !cs.state().is_complete()
                    && let Some(last_snapshot) = cs.last_snapshot()
                    && last_snapshot.energy < energy
                {
                    // note: the CP only provides one connector
                    // so don't filter on whether this MeterValues
                    // comes from connector 0 or 1, it's useful for us anyway
                    // in this state
                    self.update_current_session(mv, energy);
                }

                info!(">> MeterValue with energy set to probation");
            }
            Unknown => unreachable!("energy added"),
        }
    }

    pub fn update_current_session(&mut self, mv: MeterValueSelection, energy: u64) {
        let Some(cs) = self.charging_session.as_mut() else {
            return;
        };

        let remain_sched = self.charging_schedule.as_ref().map(|s| {
            s.remaining(
                chrono::Local::now().naive_local(),
                self.bms.constant_power_loss,
            )
        });

        let snapshot = ChargingSessionSnapshot::builder(mv.timestamp, energy)
            .power(mv.active_power_import)
            .l1_voltage(mv.voltage_l1)
            .temperature(mv.temperature)
            .build();
        let soc_progress = cs.add_snapshot(snapshot);
        if soc_progress.is_complete()
            && !cs.is_complete()
            // don't stop if we are nearly done with the schedule
            // (less than 1% here) so we can start a new one without unplugging
            // FIXME could be an option
            // FIXME implement an optional intermediate SoC target for multi-period scheds
            && remain_sched.is_none_or(|outstg| outstg.energy > self.bms.capacity / 100.0)
        {
            info!(
                "## Stopping session {}: {soc_progress}",
                cs.transaction_id()
            );
            cs.set_state(ChargingSessionState::SoCCapReached);
            self.command_queue
                .push_back(CommandToChargingPoint::StopTransaction(cs.transaction_id()));
        }

        self.notif_session_progress();
    }

    pub fn have_dpm_data(&mut self, dpm_data: DpmSelection) {
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

    /// Call this when a permanent 0 W schedule was set
    ///
    /// This can occur on reboot for instance
    // FIXME re-set the charging schedule after boot instead
    pub fn permanent_0w_set(&mut self) {
        if let Some(mut schedule) = self.charging_schedule.take() {
            schedule.inactivate();
        };
        let mut schedule = ChargingSchedule::new();
        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule);
    }

    fn notif_session_progress(&self) {
        let Some(ref cs) = self.charging_session else {
            return;
        };

        let remain_sched = self.charging_schedule.as_ref().map(|s| {
            s.remaining(
                chrono::Local::now().naive_local(),
                self.bms.constant_power_loss,
            )
        });

        info!(
            "## session {cs} / {}{}",
            SoCProgress::from_soc_and_cap(cs.last_soc(), self.bms.soc_cap).cap(),
            if let Some(remain_sched) = remain_sched {
                format!(
                    ", remaining: {remain_sched}{}",
                    if !remain_sched.is_zero() {
                        format!(", {:.1} %", remain_sched.energy / self.bms.capacity * 100.0)
                    } else {
                        "".to_string()
                    }
                )
            } else {
                "".to_string()
            }
        );

        use ChargingSessionState::*;
        use notification::ChargeState;
        let charge_state = match *cs.state() {
            Preparing => ChargeState::Preparing,
            Charging | SuspendedByEvse | SoCCapReached => {
                let charge_progress = notification::ChargeProgress {
                    soc: (cs.last_soc().absolute().unwrap_or_default().clamp(0.0, 1.0) * 100.0)
                        .round() as u8,
                    target_soc: (self.bms.soc_cap.unwrap_or_default().clamp(0.0, 1.0) * 100.0)
                        .round() as u8,
                    seconds_left: remain_sched.map_or_default(|rs| rs.duration).num_seconds()
                        as u16,
                };
                match *cs.state() {
                    Charging => notification::ChargeState::Charging(charge_progress),
                    SuspendedByEvse | SoCCapReached => {
                        notification::ChargeState::SuspendedEvse(charge_progress)
                    }
                    _ => unreachable!(),
                }
            }
            SuspendedByEv => ChargeState::SuspendedEv,
            Error(_) => ChargeState::Error,
            _ => return,
        };

        self.notif_state(charge_state);
    }

    fn notif_state(&self, state: notification::ChargeState) {
        trace!("notifiying {state:?}");
        if let Err(err) = self.notif_tx.send(ChargePointNotification::Charge(state)) {
            error!("error sending notification: {err}");
        }
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

    pub fn pop_command(&mut self) -> Option<CommandToChargingPoint> {
        self.command_queue.pop_front()
    }
}
