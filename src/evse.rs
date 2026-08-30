use chrono::Utc;
use log::*;
use ocpp_rs::v16::{call, enums::*};
use std::collections::VecDeque;

use crate::{
    Bms, ChargingPlan, ChargingSchedule, ChargingSession, ChargingSessionSnapshot,
    ChargingSessionState, CommandToChargingPoint, Database, SoC, bms::SoCProgress, measurements::*,
};

#[derive(Debug)]
pub struct Evse {
    connector_id: Option<u32>,
    bms: Bms,
    last_known_tid: i32,
    energy_tracker: EnergyTracker,
    last_known_stop_energy: Option<u64>,
    meter_value_observer: MeterValueObserver,
    last_dpm: Option<DpmSelection>,
    command_queue: VecDeque<CommandToChargingPoint>,
    charging_session: Option<ChargingSession>,
    charging_schedule: Option<ChargingSchedule>,
}

impl Evse {
    pub fn new(
        bms: Bms,
        last_charging_session: Option<ChargingSession>,
        mut last_charging_schedule: Option<ChargingSchedule>,
    ) -> Self {
        let mut this = Evse {
            connector_id: None,
            bms,
            last_known_tid: 0,
            last_known_stop_energy: None,
            energy_tracker: Default::default(),
            meter_value_observer: Default::default(),
            last_dpm: None,
            command_queue: VecDeque::new(),
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

                let action_ts = status.timestamp.map_or_else(Utc::now, |ts| ts.inner());
                if matches!(status.status, ChargePointStatus::Charging) {
                    warn!("{connector_ok_log}");
                } else {
                    info!("{connector_ok_log}");
                }

                match status.status {
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
                                status.status,
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
                            cs.set_state(status.status.clone());
                            self.log_session_progress();
                        }
                    }
                    ChargePointStatus::SuspendedEV | ChargePointStatus::Faulted => {
                        // FIXME reached 100% => not restarting the session?
                        if let Some(mut cs) = self.charging_session.take() {
                            cs.set_state(status.status.clone());
                            self.log_session_progress();
                        }
                    }
                    ChargePointStatus::Unavailable | ChargePointStatus::Reserved => (),
                }
            }
            if self.connector_id.is_none() {
                self.connector_id = Some(status.connector_id);
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
                cs.stop(stop.timestamp.inner(), stop.meter_stop, stop.reason)
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
                stop.reason,
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
                            self.command_queue
                                .push_back(CommandToChargingPoint::StopTransaction(transaction_id));
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

    pub fn pop_command(&mut self) -> Option<CommandToChargingPoint> {
        self.command_queue.pop_front()
    }
}
