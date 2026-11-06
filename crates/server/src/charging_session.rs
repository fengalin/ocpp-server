use chrono::{DateTime, Utc};
use ocpp_rs::v16::enums;
use std::{fmt, str};
use tracing::{debug, error};

use charge_system_common_model::bms::{Bms, SoC, SoCProgress};

use crate::Database;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Utc>,
    /// whether this snapshot should be used as a reference
    /// for added energy / soc calculation
    pub is_reference: bool,
    pub energy: u64,
    pub power: Option<u32>,
    pub l1_voltage: Option<u16>,
    pub temperature: Option<u16>,
    pub soc: SoC,
    pub target_soc: Option<f32>,
}
impl ChargingSessionSnapshot {
    pub fn new(timestamp: DateTime<Utc>, energy: u64) -> Self {
        ChargingSessionSnapshot {
            timestamp,
            is_reference: false,
            energy,
            power: None,
            l1_voltage: None,
            temperature: None,
            soc: SoC::Unknown,
            target_soc: None,
        }
    }

    pub fn builder(timestamp: DateTime<Utc>, energy: u64) -> ChargingSessionSnapshotBuilder {
        ChargingSessionSnapshotBuilder(Self::new(timestamp, energy))
    }
}
pub struct ChargingSessionSnapshotBuilder(ChargingSessionSnapshot);
impl ChargingSessionSnapshotBuilder {
    pub fn is_reference(mut self, is_reference: bool) -> Self {
        self.0.is_reference = is_reference;
        self
    }
    pub fn power(mut self, power: impl Into<Option<u32>>) -> Self {
        self.0.power = power.into();
        self
    }
    pub fn l1_voltage(mut self, l1_voltage: impl Into<Option<u16>>) -> Self {
        self.0.l1_voltage = l1_voltage.into();
        self
    }
    pub fn temperature(mut self, temperature: impl Into<Option<u16>>) -> Self {
        self.0.temperature = temperature.into();
        self
    }
    pub fn soc(mut self, soc: SoC) -> Self {
        self.0.soc = soc;
        self
    }
    pub fn target_soc(mut self, target_soc: impl Into<Option<f32>>) -> Self {
        self.0.target_soc = target_soc.into();
        self
    }
    pub fn build(self) -> ChargingSessionSnapshot {
        self.0
    }
}

#[derive(Debug, PartialEq)]
pub struct ChargingSession {
    session_id: i32,
    transaction_id: i32,
    battery_capacity: u32,
    snapshots: Vec<ChargingSessionSnapshot>,
    state: ChargingSessionState,
}

impl ChargingSession {
    pub fn new(bms: &mut Bms, timestamp: DateTime<Utc>, tid: i32, energy: u64) -> Self {
        bms.update_energy(energy);

        // Finalize the first snapshot against the BMS exactly like
        // `add_and_save_snapshot` does, so it can be persisted atomically
        // with the new session.
        let snapshot = ChargingSessionSnapshot::builder(timestamp, energy)
            .is_reference(true)
            .soc(bms.current_soc())
            .target_soc(bms.target_soc)
            .build();

        let session_id = Database::get()
            .add_charging_session_with_snapshot(ChargingSessionState::Preparing, tid, &snapshot)
            .expect("persist new charging session with its first snapshot");

        ChargingSession {
            session_id,
            transaction_id: tid,
            battery_capacity: bms.capacity() as u32,
            snapshots: vec![snapshot],
            state: ChargingSessionState::Preparing,
        }
    }

    /// Builds a ChargingSession in the specified state
    ///
    /// This is useful for cases where we missed the transaction start.
    pub fn with_state(
        bms: &mut Bms,
        transaction_id: i32,
        state: impl Into<ChargingSessionState>,
        initial_energy: u64,
        timestamp: impl Into<Option<DateTime<Utc>>>,
    ) -> Self {
        let state = state.into();

        bms.update_energy(initial_energy);

        let snapshot = ChargingSessionSnapshot::builder(
            timestamp.into().unwrap_or_else(Utc::now),
            initial_energy,
        )
        .is_reference(true)
        .soc(bms.current_soc())
        .target_soc(bms.target_soc)
        .build();

        let session_id = Database::get()
            .add_charging_session_with_snapshot(state.clone(), transaction_id, &snapshot)
            .expect("persist charging session with its first snapshot");

        ChargingSession {
            session_id,
            transaction_id,
            battery_capacity: bms.capacity() as u32,
            snapshots: vec![snapshot],
            state,
        }
    }

    /// Persists an unknown ChargingSession reported as stopped
    ///
    /// This allows retrieving the last known stop energy later.
    pub fn save_missing_stopped_session(
        timestamp: Option<DateTime<Utc>>,
        reason: impl Into<ChargingSessionState>,
        stop_energy: u64,
        transaction_id: i32,
    ) {
        let state = reason.into();
        let snapshot =
            ChargingSessionSnapshot::builder(timestamp.unwrap_or_else(Utc::now), stop_energy)
                .build();
        // Session + first snapshot atomically (see
        // `Database::add_charging_session_with_snapshot`).
        if let Err(err) =
            Database::get().add_charging_session_with_snapshot(state, transaction_id, &snapshot)
        {
            error!("could not persist missing stopped session: {err}");
        }
    }

    pub fn from_database(
        battery_capacity: u32,
        session_id: i32,
        transaction_id: i32,
        state: ChargingSessionState,
    ) -> Self {
        ChargingSession {
            session_id,
            transaction_id,
            battery_capacity,
            snapshots: vec![],
            state,
        }
    }

    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    pub fn transaction_id(&self) -> i32 {
        self.transaction_id
    }

    pub fn is_complete(&self) -> bool {
        self.state.is_complete()
    }

    pub fn state(&self) -> &ChargingSessionState {
        &self.state
    }

    pub fn set_state(&mut self, state: impl Into<ChargingSessionState>) {
        let state = state.into();
        if self.state == state {
            return;
        }

        Database::get().set_charging_session_state(self.session_id, &state);
        self.state = state;
    }

    fn add_and_save_snapshot(
        &mut self,
        bms: &mut Bms,
        db: &Database,
        mut snapshot: ChargingSessionSnapshot,
    ) -> SoCProgress {
        if bms.initial_energy().is_none() {
            // TODO check this is real world situations before turning it into an assertion?
            error!("FIXME unknown BMS ref energy while adding a non-reference snapshot");
        }

        bms.update_energy(snapshot.energy);
        snapshot.soc = bms.current_soc();

        if let Err(err) = db.add_charging_session_snapshot(self.session_id, &snapshot) {
            error!("could not insert charging session snapshot: {err}");
        }
        self.snapshots.push(snapshot);

        bms.soc_progress()
    }

    pub fn add_snapshot(
        &mut self,
        bms: &mut Bms,
        snapshot: ChargingSessionSnapshot,
    ) -> SoCProgress {
        self.add_and_save_snapshot(bms, &Database::get(), snapshot)
    }

    pub fn add_snapshot_from_database(&mut self, snapshot: ChargingSessionSnapshot) {
        self.snapshots.push(snapshot);
    }

    /// Applies BMS updates to this `ChargingSession`
    ///
    /// Updates include:
    ///
    /// * setting BMS initial energy
    /// * setting BMS initial SoC (if user hasn't applied any corrections)
    /// * setting BMS current SoC (if user hasn't applied any corrections)
    /// * adding a new reference snapshot if user has applied a SoC correction
    ///
    /// Returns true if a new reference snapshot was added.
    pub fn apply_bms_updates(&mut self, db: &Database, bms: &mut Bms) {
        if self.state.is_complete() {
            return;
        }

        let Some(ref_snapshot) = self.last_reference_snapshot() else {
            error!(
                session_id = self.session_id,
                "FIXME: no reference snapshots",
            );
            return;
        };

        let Some(current_energy) = bms.current_energy() else {
            error!(session_id = self.session_id, %bms, "can't update session due to unknown BMS energy");
            return;
        };

        if ref_snapshot.soc.pratically_equals(bms.current_soc())
            && ref_snapshot.energy == current_energy
        {
            debug!(session_id = self.session_id, "last snapshot is up to date");
            return;
        }

        let new_snapshot = ChargingSessionSnapshot::builder(Utc::now(), current_energy)
            .is_reference(true)
            .soc(bms.current_soc())
            .target_soc(bms.target_soc())
            .build();

        if let Err(err) = db.add_charging_session_snapshot(self.session_id, &new_snapshot) {
            error!("could not insert charging session snapshot: {err}");
        }
        self.snapshots.push(new_snapshot);
    }

    pub fn stop(
        &mut self,
        bms: &mut Bms,
        timestamp: DateTime<Utc>,
        energy: u64,
        reason: impl Into<ChargingSessionState>,
    ) {
        let db = Database::get();

        self.add_and_save_snapshot(bms, &db, ChargingSessionSnapshot::new(timestamp, energy));
        let reason = reason.into();
        db.stop_charging_session(self.session_id, &reason);
        self.state = reason;
    }

    pub fn first_snapshot(&self) -> Option<&ChargingSessionSnapshot> {
        self.snapshots.first()
    }

    pub fn last_snapshot(&self) -> Option<&ChargingSessionSnapshot> {
        self.snapshots.last()
    }

    pub fn last_reference_snapshot(&self) -> Option<&ChargingSessionSnapshot> {
        self.snapshots.iter().rev().find(|s| s.is_reference)
    }

    pub fn last_energy(&self) -> u64 {
        self.last_snapshot()
            .expect("at least one snapshot at this stage")
            .energy
    }

    pub fn last_soc(&self) -> SoC {
        self.last_snapshot()
            .expect("at least one snapshot at this stage")
            .soc
    }

    pub fn initial_soc(&self) -> SoC {
        let Some(first_snapshot) = self.snapshots.first() else {
            return SoC::Unknown;
        };
        let Some(ref_snapshot) = self.last_reference_snapshot() else {
            return SoC::Unknown;
        };

        if ref_snapshot.soc.is_unknown() {
            return SoC::Unknown;
        }

        let first_energy = first_snapshot.energy;
        let ref_energy = ref_snapshot.energy;

        if ref_energy < first_energy {
            error!(
                "FIXME: session: {}, reference snapshots energy: {ref_energy} \
                < first snapshot energy: {first_energy}",
                self.session_id
            );
            return SoC::Unknown;
        }

        let first_to_ref_energy = ref_energy - first_energy;
        let soc_delta = (first_to_ref_energy as f32) / self.battery_capacity as f32;

        ref_snapshot.soc - soc_delta
    }
}

impl fmt::Display for ChargingSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "tid: {}, state: {}, initial SoC: {}, last SoC: {}",
            self.transaction_id,
            self.state,
            self.initial_soc(),
            self.last_soc(),
        ))
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub enum ChargingSessionState {
    Preparing,
    Charging,
    SoCTargetReached,
    SuspendedByEvse,
    SuspendedByEv,
    StoppedByUser,
    Reboot,
    UnlockCommandFromServer,
    Unknown,
    Error(String),
}

impl ChargingSessionState {
    pub fn is_complete(&self) -> bool {
        use ChargingSessionState::*;
        matches!(
            self,
            SoCTargetReached
                | SuspendedByEv
                | StoppedByUser
                | Reboot
                | UnlockCommandFromServer
                | Error(_)
        )
    }
}

impl fmt::Display for ChargingSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ChargingSessionState::*;
        f.write_str(match self {
            Preparing => "Preparing",
            Charging => "Charging",
            SoCTargetReached => "SoCCapReached",
            SuspendedByEvse => "SuspendedByEvse",
            SuspendedByEv => "SuspendedByEv",
            StoppedByUser => "StoppedByUser",
            Reboot => "Reboot",
            UnlockCommandFromServer => "UnlockCommandFromServer",
            Unknown => "Unknown",
            Error(err) => {
                return write!(f, "{err}");
            }
        })
    }
}

impl str::FromStr for ChargingSessionState {
    // use ! when stable
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use ChargingSessionState::*;
        Ok(match s {
            "Preparing" => Preparing,
            "Charging" => Charging,
            "SoCCapReached" => SoCTargetReached,
            "SuspendedByEvse" => SuspendedByEvse,
            "SuspendedByEv" => SuspendedByEv,
            "StoppedByUser" => StoppedByUser,
            "Reboot" => Reboot,
            "UnlockCommandFromServer" => UnlockCommandFromServer,
            "Unknown" => Unknown,
            other => Error(other.to_string()),
        })
    }
}

impl From<enums::ChargePointStatus> for ChargingSessionState {
    fn from(cp_status: enums::ChargePointStatus) -> Self {
        use ChargingSessionState::*;
        use enums::ChargePointStatus::*;
        match cp_status {
            enums::ChargePointStatus::Preparing => ChargingSessionState::Preparing,
            enums::ChargePointStatus::Charging => ChargingSessionState::Charging,
            SuspendedEVSE => SuspendedByEvse,
            SuspendedEV => SuspendedByEv,
            Finishing => SoCTargetReached,
            Unavailable => Error("Unavailable".to_string()),
            Faulted => Error("Faulted".to_string()),
            Available => panic!("should not be called in this state"),
            Reserved => unimplemented!(),
            enums::ChargePointStatus::Unknown(_) => ChargingSessionState::Unknown,
        }
    }
}

impl From<Option<&enums::Reason>> for ChargingSessionState {
    fn from(reason: Option<&enums::Reason>) -> Self {
        use ChargingSessionState::*;
        let Some(reason) = reason else {
            return Error("stopped for unknown reason".to_string());
        };
        use enums::Reason::*;
        match reason {
            Local => StoppedByUser,
            Remote => StoppedByUser,
            DeAuthorized => Error("de-authorized".to_string()),
            EmergencyStop => Error("emergency stop".to_string()),
            EVDisconnected => Error("EV disconnected".to_string()),
            HardReset => Error("hard reset".to_string()),
            SoftReset => Error("soft reset".to_string()),
            PowerLoss => Error("power loss".to_string()),
            UnlockCommand => UnlockCommandFromServer,
            enums::Reason::Reboot => ChargingSessionState::Reboot,
            Other => Error("other".to_string()),
            enums::Reason::Unknown(code) => Error(format!("unknown({code}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_system_common_model::bms;

    const INITIAL_ENERGY: u64 = 100;
    const MAX_ADDED_ENERGY_FIRST_PERIOD: u64 = 25;

    #[test]
    fn charging_session() {
        crate::tests::init();

        // run the tests sequentially so get_last_active_charging_session
        // returns what it is supposed to return
        session_with_soc_and_target();
        session_unknown_initial_soc();
        session_unknown_initial_soc_no_target();
        multi_period_session_with_soc_and_target();
        multi_period_session_unknown_initial_soc();
        missed_start_active_session_with_soc_and_target();
        missed_start_suspended_session_with_soc_and_target();
    }

    fn session_with_soc_and_target() {
        let start_time = Utc::now();
        let initial_soc = 0.3;
        let target =
            initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(initial_soc)
            .target_soc(target)
            .build()
            .unwrap();
        let tid = 10;
        let mut cs = ChargingSession::new(&mut bms, start_time, tid, INITIAL_ENERGY);
        assert_eq!(tid, cs.transaction_id());
        assert_eq!(initial_soc, bms.initial_soc().unwrap());
        assert_eq!(Some(target), bms.target_soc);
        let last_snapshot = cs.last_snapshot().unwrap();
        assert_eq!(last_snapshot.timestamp, start_time);
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.energy, INITIAL_ENERGY);

        let snapshot_ts = Utc::now();
        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(snapshot_ts, INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteTargetNotReached {
                soc: initial_soc + 10.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        let last_snapshot = cs.last_snapshot().unwrap();
        assert_eq!(last_snapshot.timestamp, snapshot_ts);
        assert!(!last_snapshot.is_reference);
        assert_eq!(last_snapshot.energy, INITIAL_ENERGY + 10);
        assert_eq!(last_snapshot.power, Some(10));
        assert_eq!(last_snapshot.l1_voltage, Some(230));
        assert_eq!(last_snapshot.temperature, Some(27));

        let snapshot_ts = Utc::now();
        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(snapshot_ts, INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteTargetNotReached {
                soc: initial_soc + 20.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        let last_snapshot = cs.last_snapshot().unwrap();
        assert_eq!(last_snapshot.timestamp, snapshot_ts);
        assert!(!last_snapshot.is_reference);
        assert_eq!(last_snapshot.energy, INITIAL_ENERGY + 20);
        assert_eq!(last_snapshot.power, Some(10));
        assert_eq!(last_snapshot.l1_voltage, Some(228));
        assert_eq!(last_snapshot.temperature, Some(28));

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::Preparing,
            last_session.unwrap().state()
        );

        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(
                Utc::now(),
                INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            )
            .power(10)
            .l1_voltage(227)
            .temperature(30)
            .build(),
        );
        assert!(soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteTargetReached {
                soc: initial_soc
                    + MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
        cs.stop(
            &mut bms,
            Utc::now(),
            INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            ChargingSessionState::SoCTargetReached,
        );

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SoCTargetReached,
            last_session.unwrap().state()
        );
    }

    fn session_unknown_initial_soc() {
        let start_time = Utc::now();
        let target = MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .target_soc(target)
            .build()
            .unwrap();
        let tid = 11;
        let mut cs = ChargingSession::new(&mut bms, start_time, tid, INITIAL_ENERGY);
        assert_eq!(tid, cs.transaction_id());
        assert_eq!(None, bms.initial_soc());
        assert_eq!(Some(target), bms.target_soc);
        assert!(cs.last_snapshot().unwrap().is_reference);

        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::RelativeTargetNotReached {
                rel_soc: 10.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::RelativeTargetNotReached {
                rel_soc: 20.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::Preparing,
            last_session.unwrap().state()
        );

        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(
                Utc::now(),
                INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            )
            .power(10)
            .l1_voltage(227)
            .temperature(30)
            .build(),
        );
        assert!(soc_progress.is_complete());
        assert_eq!(
            SoCProgress::RelativeCapReached {
                rel_soc: MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32,
                cap: target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
        cs.stop(
            &mut bms,
            Utc::now(),
            INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            ChargingSessionState::SoCTargetReached,
        );

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SoCTargetReached,
            last_session.unwrap().state()
        );
    }

    fn session_unknown_initial_soc_no_target() {
        let start_time = Utc::now();

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY).build().unwrap();
        let tid = 14;
        let mut cs = ChargingSession::new(&mut bms, start_time, tid, INITIAL_ENERGY);
        assert_eq!(tid, cs.transaction_id());
        assert_eq!(None, bms.initial_soc());
        assert!(bms.target_soc.is_none());
        assert!(cs.last_snapshot().unwrap().is_reference);

        assert_eq!(None, bms.initial_soc());
        assert_eq!(
            SoCProgress::RelativeUncapped(10.0 / bms::DEFAULT_CAPACITY as f32),
            cs.add_snapshot(
                &mut bms,
                ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert_eq!(
            SoCProgress::RelativeUncapped(20.0 / bms::DEFAULT_CAPACITY as f32),
            cs.add_snapshot(
                &mut bms,
                ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::Preparing,
            last_session.unwrap().state()
        );

        assert_eq!(
            Some(&cs),
            Database::get()
                .get_last_charging_session(&mut bms)
                .unwrap()
                .as_ref(),
        );

        cs.stop(
            &mut bms,
            Utc::now(),
            INITIAL_ENERGY + 25,
            ChargingSessionState::SuspendedByEv,
        );

        let last_session = Database::get().get_last_charging_session(&mut bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SuspendedByEv,
            last_session.unwrap().state()
        );
    }

    fn multi_period_session_with_soc_and_target() {
        let start_time = Utc::now();
        let initial_soc = 0.3;
        let target =
            initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(initial_soc)
            .target_soc(target)
            .build()
            .unwrap();
        let tid = 15;
        let mut cs = ChargingSession::new(&mut bms, start_time, tid, INITIAL_ENERGY);
        assert_eq!(tid, cs.transaction_id());
        assert!(cs.last_snapshot().unwrap().is_reference);

        let _ = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        let _ = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
    }

    fn multi_period_session_unknown_initial_soc() {
        let start_time = Utc::now();
        let mut target = MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .target_soc(target)
            .build()
            .unwrap();
        let tid = 16;
        let mut cs = ChargingSession::new(&mut bms, start_time, tid, INITIAL_ENERGY);
        assert_eq!(tid, cs.transaction_id());
        assert!(cs.last_snapshot().unwrap().is_reference);

        let _ = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        let _ = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        // Simulate server going offline, then reconnecting without the session being suspended
        // not changing initial SoC and expending the SoC cap
        target += 0.01;
        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .target_soc(target)
            .build()
            .unwrap();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&mut bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(Some(INITIAL_ENERGY), bms.initial_energy());
        // recovered session started from an unknown reference SoC
        assert_eq!(None, bms.initial_soc());
        assert_eq!(Some(target), bms.target_soc);

        let soc_progress = recovered_session.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 30)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert_eq!(
            SoCProgress::RelativeTargetNotReached {
                rel_soc: 30.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
    }

    /// Simulates receiving a MeterValue with active enery import
    fn missed_start_active_session_with_soc_and_target() {
        let initial_soc = 0.3;
        let target =
            initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(initial_soc)
            .target_soc(target)
            .build()
            .unwrap();

        let current_time = Utc::now();
        let last_known_energy = INITIAL_ENERGY;
        let tid = 12;
        let mut cs = ChargingSession::with_state(
            &mut bms,
            tid,
            ChargingSessionState::Unknown,
            last_known_energy,
            current_time,
        );
        assert_eq!(tid, cs.transaction_id());
        assert_eq!(ChargingSessionState::Unknown, cs.state);

        let last_snapshot = cs.last_snapshot().unwrap();
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.timestamp, current_time);
        assert_eq!(last_snapshot.energy, last_known_energy);

        assert_eq!(Some(last_known_energy), bms.initial_energy());
        assert_eq!(Some(initial_soc), bms.initial_soc());
        assert_eq!(Some(target), bms.target_soc);

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .target_soc(target)
            .build()
            .unwrap();
        let last_session = Database::get()
            .get_last_charging_session(&mut bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);

        cs.set_state(ChargingSessionState::Preparing);
        assert_eq!(cs.state, ChargingSessionState::Preparing);

        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteTargetNotReached {
                soc: initial_soc + 10.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get()
            .get_last_charging_session(&mut bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);
    }

    fn missed_start_suspended_session_with_soc_and_target() {
        let initial_soc = 0.3;
        let target =
            initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f32 / bms::DEFAULT_CAPACITY as f32;

        let mut bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(initial_soc)
            .target_soc(target)
            .build()
            .unwrap();

        let current_time = Utc::now();
        let current_energy = INITIAL_ENERGY;
        let tid = 13;
        let mut cs = ChargingSession::with_state(
            &mut bms,
            tid,
            enums::ChargePointStatus::SuspendedEVSE,
            current_energy,
            current_time,
        );
        assert_eq!(cs.state, ChargingSessionState::SuspendedByEvse);
        assert_eq!(tid, cs.transaction_id());

        let last_snapshot = cs.last_snapshot().unwrap();
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.timestamp, current_time);
        assert_eq!(last_snapshot.energy, current_energy);

        assert_eq!(Some(current_energy), bms.initial_energy());
        assert_eq!(Some(initial_soc), bms.initial_soc());
        assert_eq!(Some(target), bms.target_soc);

        let last_session = Database::get()
            .get_last_charging_session(&mut bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);

        cs.set_state(ChargingSessionState::Preparing);
        assert_eq!(cs.state, ChargingSessionState::Preparing);

        let soc_progress = cs.add_snapshot(
            &mut bms,
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteTargetNotReached {
                soc: initial_soc + 10.0 / bms::DEFAULT_CAPACITY as f32,
                target,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get()
            .get_last_charging_session(&mut bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);
    }
}
