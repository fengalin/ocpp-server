use chrono::{Local, Utc};
use log::*;
use ocpp_rs::v16::{call, enums::*};
use tokio::sync::broadcast;

use std::collections::VecDeque;

use charge_system_common_model::notification::{self, Notification};
use charge_system_common_model::{
    bms::{self, Bms, SoC, SoCProgress},
    meter_value::MeterValueSelection,
    model::Ts,
    plan_model::{AppliedChargingPlan, ChargingPeriod, ChargingPlan},
};

use crate::{
    ChargingSchedule, ChargingSession, ChargingSessionSnapshot, ChargingSessionState,
    CommandToChargingPoint, Database, measurements::*,
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
    /// The last `StatusNotification` had status `Finishing`: on the next
    /// `Available`, apply the default plan (see the status handler below).
    saw_finishing: bool,
    notif_tx: broadcast::Sender<Notification>,
}

impl Evse {
    pub fn new(mut bms: Bms, notif_tx: broadcast::Sender<Notification>) -> Self {
        let (last_charging_session, mut last_charging_schedule) = {
            let db = Database::get();
            (
                db.get_last_charging_session(&mut bms)
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
            saw_finishing: false,
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

        // FIXME also notify charging schedule & details about the session
        if let Err(err) = this
            .notif_tx
            .send(Notification::BmsUpdate(this.bms.clone()))
        {
            error!("error broadcasting BMS update: {err}");
        }

        this
    }

    /// Updates the BMS initial SoC.
    pub fn set_initial_soc(&mut self, soc: f32) {
        info!("## setting initial SoC: {soc}");
        self.bms.initial_soc.update(SoC::new_absolute(soc));

        if let Some(cs) = self.charging_session.as_mut() {
            cs.apply_bms_updates(&Database::get(), &mut self.bms);
        }

        info!("## broadcasting BMS update: {}", self.bms);
        if let Err(err) = self
            .notif_tx
            .send(Notification::BmsUpdate(self.bms.clone()))
        {
            error!("error broadcasting BMS update: {err}");
        }
    }

    pub fn set_charging_plan(&mut self, charging_plan: ChargingPlan) {
        if let Some(mut prev_schedule) = self.charging_schedule.take() {
            prev_schedule.inactivate();
        };

        // The BMS target SoC mirrors the requested plan: plans with a charging
        // target set it (the web app and the webserver's world get it through
        // the ChargingPlan notification below — no separate BmsUpdate on this
        // path), plans without one drop any stale value. The install paths
        // never touch it, so a computation failure that ends up on Blocking
        // keeps the target the user just entered (no clearing, no re-entry).
        match &charging_plan {
            ChargingPlan::Deadline { target_soc, .. } | ChargingPlan::Asap { target_soc, .. } => {
                info!(
                    "applying charging plan with target SoC: {:.1}",
                    *target_soc * 100.0
                );
                self.bms.target_soc = Some(*target_soc);
            }
            _ => {
                self.bms.target_soc = None;
            }
        }

        match ChargingSchedule::try_from_charging_plan(&charging_plan, &self.bms) {
            Ok(Some(schedule)) => self.apply_computed_schedule(charging_plan, schedule),
            // An explicit `Unlimited` plan clears any profile installed on the
            // CP (unrestricted charging).
            Ok(None) => {
                info!("## applying unlimited plan (no schedule)");
                self.command_queue
                    .push_back(CommandToChargingPoint::ClearChargingProfile);
                self.notify_charging_plan(AppliedChargingPlan {
                    plan: charging_plan,
                    periods: vec![],
                });
            }
            // Any other plan whose schedule couldn't be computed must NOT leave
            // the CP unrestricted: install the default (Blocking) plan instead.
            Err(err) => {
                warn!(
                    "## error computing charging schedule for {charging_plan:?}; \
                    applying the default (blocking) plan: {err}"
                );
                self.apply_computed_schedule(
                    ChargingPlan::Blocking,
                    ChargingSchedule::new_blocking(),
                );
                // Clients mirror the target from plan notifications (the one
                // above would clear it for Blocking): carry the kept value in a
                // follow-up BmsUpdate.
                if let Err(err) = self
                    .notif_tx
                    .send(Notification::BmsUpdate(self.bms.clone()))
                {
                    error!("error broadcasting BMS update: {err}");
                }
            }
        }
    }

    /// Installs a plan whose schedule is already computed: persists it, queues
    /// the `SetChargingProfile`, and notifies clients of the applied plan + its
    /// computed schedule. The caller inactivated the previous schedule and
    /// mirrored the BMS target (the Deadline/Asap variants carry it in the
    /// notification, so no BmsUpdate is needed here).
    fn apply_computed_schedule(&mut self, plan: ChargingPlan, mut schedule: ChargingSchedule) {
        let periods = schedule_periods(&schedule);
        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule.clone());
        self.command_queue
            .push_back(CommandToChargingPoint::SetChargingSchedule(schedule));
        self.notify_charging_plan(AppliedChargingPlan { plan, periods });
    }

    /// Broadcasts the current plan state (plan + computed schedule) to all
    /// notification consumers.
    fn notify_charging_plan(&self, applied: AppliedChargingPlan) {
        if let Err(err) = self.notif_tx.send(Notification::ChargingPlan(applied)) {
            error!("error broadcasting charging plan notification: {err}");
        }
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
            .push_back(CommandToChargingPoint::StopTransaction);
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
            &mut self.bms,
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
                                &mut self.bms,
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
                            if self.saw_finishing {
                                // The session just finished (Finishing ->
                                // Available): the CP forgets its charging
                                // profile on EV disconnect, and we don't want
                                // it to start a new unmanaged session on the
                                // next plug-in — apply the default plan
                                // (Blocking) regardless of the plan in effect.
                                info!(
                                    "## applying the default (blocking) plan \
                                    after Finishing -> Available"
                                );
                                self.set_charging_plan(ChargingPlan::Blocking);
                            }
                            self.notif_state(notification::ChargeStatus::Available);
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
                                cs.stop(
                                    &mut self.bms,
                                    status_ts,
                                    cs.last_energy(),
                                    ChargePointStatus::Finishing,
                                );
                                self.notif_session_progress();
                            } else {
                                cs.set_state(status.status.clone());
                                self.notif_session_progress();
                            }
                        } else {
                            self.notif_state(notification::ChargeStatus::Finishing);
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
                            self.notif_state(notification::ChargeStatus::Preparing);
                        } else {
                            // FIXME start a new session instead
                            self.notif_state(notification::ChargeStatus::UnknownSession);
                        }
                    }
                    ChargePointStatus::SuspendedEV => {
                        // FIXME reached 100% => not restarting the session?
                        if let Some(ref mut cs) = self.charging_session {
                            cs.set_state(status.status.clone());
                            self.notif_session_progress();
                        } else {
                            self.notif_state(notification::ChargeStatus::SuspendedEv);
                        }
                    }
                    ChargePointStatus::Faulted => {
                        // FIXME reached 100% => not restarting the session?
                        if let Some(ref mut cs) = self.charging_session {
                            cs.set_state(status.status.clone());
                            self.notif_session_progress();
                        } else {
                            self.notif_state(notification::ChargeStatus::Error);
                        }
                    }
                    _ => {
                        warn!("unhandled charging point status: {:?}", status.status);
                        self.notif_state(notification::ChargeStatus::Error);
                    }
                }

                // Remember Finishing: it is the trigger for applying the
                // default plan on the following Available.
                self.saw_finishing = matches!(status.status, ChargePointStatus::Finishing);
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
                &mut self.bms,
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
            &mut self.bms,
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

        // A new session changed the BMS (initial/current energy) — broadcast
        // it so clients' session-energy bookkeeping (current − initial) is
        // correct from the first status event.
        info!("## broadcasting BMS update: {}", self.bms);
        if let Err(err) = self
            .notif_tx
            .send(Notification::BmsUpdate(self.bms.clone()))
        {
            error!("error broadcasting BMS update: {err}");
        }

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
                    &mut self.bms,
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
                    &mut self.bms,
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

    pub fn have_charging_point_meter_values(
        &mut self,
        mut mv: MeterValueSelection,
        tid: Option<i32>,
    ) {
        self.meter_value_observer.consolidate(&mut mv);

        if self.meter_value_observer.pertinent_to_user(&mv) {
            info!(">> MeterValues {mv}");
        } else {
            trace!(">> MeterValues {mv:?}");
        }

        self.maybe_have_energy(&mv, tid);

        // Always notif, it adds data points in case user just connected
        if let Err(err) = self.notif_tx.send(Notification::MeterValues(mv)) {
            error!("error sending notification: {err}");
        }
    }

    fn maybe_have_energy(&mut self, mv: &MeterValueSelection, tid: Option<i32>) {
        if let Some(energy) = mv.energy {
            use EnergyTracker::*;
            self.energy_tracker.have_energy(energy);
            match self.energy_tracker {
                Increasing(_) => {
                    let Some(tid) = tid else {
                        debug!(
                            "## MeterValue without transaction id & increasing energy, \
                            waiting for next MeterValue"
                        );
                        return;
                    };

                    match self.charging_session.as_mut() {
                        Some(cs) if cs.transaction_id() == tid => {
                            self.update_current_session(mv, energy);
                            return;
                        }
                        Some(cs) => {
                            warn!(
                                "## MeterValues transaction id mismatch {tid}, \
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

                    // FIXME changes below operate on a copy...
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
                        "## adding new charging session for {tid}, \
                        increasing start energy: {start_energy}"
                    );
                    self.have_transaction_id(tid);
                    self.charging_session = Some(ChargingSession::with_state(
                        &mut bms,
                        tid,
                        ChargingSessionState::Preparing,
                        energy,
                        mv.ts.to_utc(),
                    ));
                }
                Stationnary(_) => {
                    let Some(tid) = tid else {
                        debug!(
                            "## MeterValue without transaction id & stationary \
                            waiting for next MeterValue"
                        );
                        return;
                    };

                    match self.charging_session.as_mut() {
                        Some(cs) if cs.transaction_id() == tid => {
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
                                "## MeterValues transaction id mismatch {tid}, \
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
                                for unknown session: {tid}"
                            );
                        }
                    }

                    self.last_known_stop_energy = Some(energy);

                    warn!(
                        "## adding new charging session for {tid}, \
                        stationnary start energy: {:.3} kWh",
                        energy as f64 / 1_000.0
                    );
                    self.have_transaction_id(tid);
                    self.charging_session = Some(ChargingSession::with_state(
                        &mut self.bms,
                        tid,
                        ChargingSessionState::Preparing,
                        energy,
                        mv.ts.to_utc(),
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
    }

    pub fn update_current_session(&mut self, mv: &MeterValueSelection, energy: u64) {
        let Some(cs) = self.charging_session.as_mut() else {
            return;
        };

        let remain_sched = self
            .charging_schedule
            .as_ref()
            .map(|s| s.remaining(chrono::Local::now().naive_local(), bms::STATIC_LOSS));

        let snapshot = ChargingSessionSnapshot::builder(mv.ts.to_utc(), energy)
            .power(mv.power)
            .l1_voltage(mv.voltage)
            .temperature(mv.temperature)
            .build();

        let soc_progress = cs.add_snapshot(&mut self.bms, snapshot);

        if soc_progress.is_complete()
            && !cs.is_complete()
            // don't stop if we are nearly done with the schedule
            // (less than 1% here) so we can start a new one without unplugging
            // FIXME could be an option
            // FIXME implement an optional intermediate SoC target for multi-period scheds
            && remain_sched.is_none_or(|remain| remain.energy as f32 > self.bms.capacity as f32 / 100.0)
        {
            info!(
                "## Stopping session {}: {soc_progress}",
                cs.transaction_id()
            );
            cs.set_state(ChargingSessionState::SoCTargetReached);
            self.command_queue
                .push_back(CommandToChargingPoint::StopTransaction);
        }

        self.notif_session_progress();
    }

    pub fn have_dpm_data(&mut self, dpm_data: DpmSelection) {
        if dpm_data.power.is_some()
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
        let mut schedule = ChargingSchedule::new_blocking();
        schedule.id = Database::get().add_new_charging_schedule(&schedule);
        self.charging_schedule = Some(schedule);
    }

    fn notif_session_progress(&self) {
        let Some(ref cs) = self.charging_session else {
            return;
        };

        let remain_sched = self
            .charging_schedule
            .as_ref()
            .map(|s| s.remaining(chrono::Local::now().naive_local(), bms::STATIC_LOSS));

        info!(
            "## session {cs} / {}{}",
            SoCProgress::from_soc_and_cap(cs.last_soc(), self.bms.target_soc).cap(),
            if let Some(remain_sched) = remain_sched {
                format!(
                    ", remaining: {remain_sched}{}",
                    if !remain_sched.is_zero() {
                        format!(
                            ", {:.1} %",
                            remain_sched.energy as f32 / self.bms.capacity as f32 * 100.0
                        )
                    } else {
                        "".to_string()
                    }
                )
            } else {
                "".to_string()
            }
        );

        use ChargingSessionState::*;
        use notification::ChargeStatus;
        let charge_state = match *cs.state() {
            Preparing => ChargeStatus::Preparing,
            Charging | SuspendedByEvse | SoCTargetReached => {
                let charge_progress = notification::ChargeProgress {
                    soc: cs
                        .last_soc()
                        .absolute()
                        .map_or_default(|s| (s * 1_000.0).round() / 1_000.0)
                        .clamp(0.0, 1.0),
                    remaining_time: remain_sched.map_or_default(|rs| rs.duration).num_seconds()
                        as u32,
                };
                match *cs.state() {
                    Charging => notification::ChargeStatus::Charging(charge_progress),
                    SuspendedByEvse | SoCTargetReached => {
                        notification::ChargeStatus::SuspendedEvse(charge_progress)
                    }
                    _ => unreachable!(),
                }
            }
            SuspendedByEv => ChargeStatus::SuspendedEv,
            Error(_) => ChargeStatus::Error,
            _ => return,
        };

        self.notif_state(charge_state);
    }

    fn notif_state(&self, state: notification::ChargeStatus) {
        trace!("notifiying {state:?}");
        if let Err(err) = self.notif_tx.send(Notification::Status(state)) {
            error!("error sending notification: {err}");
        }
    }

    pub fn last_known_transaction_id(&mut self) -> i32 {
        self.last_known_tid
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

/// The schedule's periods as shared `ChargingPeriod`s (computed in local
/// time, converted to `Ts` for the notification).
fn schedule_periods(schedule: &ChargingSchedule) -> Vec<ChargingPeriod> {
    schedule
        .periods()
        .filter_map(|p| {
            let start = p
                .start
                .and_local_timezone(Local)
                .earliest()?
                .with_timezone(&Utc);
            Some(ChargingPeriod {
                start_at: Ts::from_utc(start),
                duration: (p.end - p.start).num_seconds().max(0) as u64,
                power_limit: p.limit,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan whose schedule cannot be computed (here: the BMS current SoC is
    /// relative, so the energy to add isn't) must fall back to the default
    /// (Blocking) plan — never leave the CP unrestricted — and must keep the
    /// target SoC this request set (what the user just entered): not clear it,
    /// not roll it back to an older value (carried to clients by a follow-up
    /// BmsUpdate). Constructed directly (not via `Evse::new`) so no session is
    /// restored from the test database and can overwrite the BMS.
    #[test]
    fn uncomputable_plan_falls_back_to_blocking() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut evse = Evse {
            bms: Bms::builder(bms::DEFAULT_CAPACITY)
                .current_soc(SoC::Relative(0.41))
                .target_soc(0.60) // a stale value the request is about to replace
                .build(),
            last_known_tid: 0,
            last_known_stop_energy: None,
            energy_tracker: Default::default(),
            meter_value_observer: Default::default(),
            last_dpm: None,
            command_queue: VecDeque::new(),
            charging_session: None,
            charging_schedule: None,
            saw_finishing: false,
            notif_tx: tx,
        };

        evse.set_charging_plan(ChargingPlan::Asap {
            target_soc: 0.80,
            power_limit: 7_400,
        });

        // The fallback installs the permanent (empty) schedule...
        let sched = evse
            .charging_schedule
            .as_ref()
            .expect("blocking schedule installed");
        assert!(sched.is_empty());
        // ...keeps the target this request set (not cleared, not rolled back
        // to the previous 0.60)...
        assert_eq!(evse.bms.target_soc, Some(0.80));
        // ...and queues its profile (not a ClearChargingProfile).
        let cmd = evse.pop_command().expect("queued command");
        assert!(matches!(
            cmd,
            CommandToChargingPoint::SetChargingSchedule(_),
        ));
        assert_eq!(evse.command_queue.len(), 0);

        // Clients get the plan notification, then a BmsUpdate carrying the kept
        // target (they mirror the target from plan notifications).
        let notif = rx.try_recv().expect("plan notification broadcast");
        match &notif {
            Notification::ChargingPlan(applied) => {
                assert!(matches!(applied.plan, ChargingPlan::Blocking));
            }
            other => panic!("expected ChargingPlan, got {other:?}"),
        }
        let notif = rx.try_recv().expect("BmsUpdate broadcast");
        match &notif {
            Notification::BmsUpdate(bms) => assert_eq!(bms.target_soc, Some(0.80)),
            other => panic!("expected BmsUpdate, got {other:?}"),
        }
    }
}
