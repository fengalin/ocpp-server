use chrono::{DateTime, Utc};
use log::{error, warn};
use ocpp_rs::v16::enums;
use std::{fmt, str};

use crate::{Bms, Database, SoC, SoCProgress};

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Utc>,
    /// whether this snapshot should be used as a reference
    /// for added energy / soc calculation
    pub is_reference: bool,
    pub energy: u64,
    pub power: Option<u64>,
    pub l1_voltage: Option<u64>,
    pub temperature: Option<u64>,
    pub soc: SoC,
    pub soc_cap: Option<f64>,
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
            soc_cap: None,
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
    pub fn power(mut self, power: impl Into<Option<u64>>) -> Self {
        self.0.power = power.into();
        self
    }
    pub fn l1_voltage(mut self, l1_voltage: impl Into<Option<u64>>) -> Self {
        self.0.l1_voltage = l1_voltage.into();
        self
    }
    pub fn temperature(mut self, temperature: impl Into<Option<u64>>) -> Self {
        self.0.temperature = temperature.into();
        self
    }
    pub fn soc(mut self, soc: SoC) -> Self {
        self.0.soc = soc;
        self
    }
    pub fn soc_cap(mut self, soc_cap: impl Into<Option<f64>>) -> Self {
        self.0.soc_cap = soc_cap.into();
        self
    }
    pub fn build(self) -> ChargingSessionSnapshot {
        self.0
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSession {
    session_id: i32,
    transaction_id: i32,
    bms: Bms,
    snapshots: Vec<ChargingSessionSnapshot>,
    state: ChargingSessionState,
}

impl ChargingSession {
    pub fn new(mut bms: Bms, timestamp: DateTime<Utc>, energy: u64) -> Self {
        let db = Database::get();

        let session_id = db.add_new_charging_session(ChargingSessionState::Active, None);

        if bms.initial_energy.is_none() {
            bms.initial_energy = Some(energy as f64);
        }

        let mut this = ChargingSession {
            session_id,
            transaction_id: session_id,
            bms,
            snapshots: vec![],
            state: ChargingSessionState::Active,
        };
        let snapshot = ChargingSessionSnapshot::builder(timestamp, energy)
            .is_reference(true)
            .soc(this.bms.initial_soc)
            .soc_cap(this.bms.soc_cap)
            .build();
        this.add_and_save_snapshot(&db, snapshot);

        this
    }

    /// Builds a ChargingSession in the specified state
    ///
    /// This is useful for cases where we missed the transaction start.
    // FIXME set BMS initial enegery by caller
    pub fn with_state(
        mut bms: Bms,
        transaction_id: i32,
        state: impl Into<ChargingSessionState>,
        start_energy: u64,
        timestamp: impl Into<Option<DateTime<Utc>>>,
    ) -> Self {
        let db = Database::get();

        let state = state.into();
        let session_id = db.add_new_charging_session(state.clone(), Some(transaction_id));

        if bms.initial_energy.is_none() {
            bms.initial_energy = Some(start_energy as f64);
            bms.current_energy = Some(start_energy as f64);
        }

        let snapshot = ChargingSessionSnapshot::builder(
            timestamp.into().unwrap_or_else(Utc::now),
            start_energy,
        )
        .is_reference(true)
        .soc(bms.initial_soc)
        .soc_cap(bms.soc_cap)
        .build();

        let mut this = ChargingSession {
            session_id,
            transaction_id,
            bms,
            snapshots: vec![],
            state,
        };
        this.add_and_save_snapshot(&db, snapshot);

        this
    }

    /// Persists an unknown ChargingSession reported as stopped
    ///
    /// This allows retrieving the last known stop energy later.
    pub fn save_missing_stopped_session(
        timestamp: Option<DateTime<Utc>>,
        reason: impl Into<ChargingSessionState>,
        stop_energy: u64,
        transaction_id: Option<i32>,
    ) {
        let db = Database::get();

        let state = reason.into();
        let session_id = db.add_new_charging_session(state.clone(), transaction_id);
        let snapshot =
            ChargingSessionSnapshot::builder(timestamp.unwrap_or_else(Utc::now), stop_energy)
                .build();
        db.add_charging_session_snapshot(session_id, &snapshot);
    }

    pub fn from_database(
        bms: Bms,
        session_id: i32,
        transaction_id: Option<i32>,
        state: ChargingSessionState,
    ) -> Self {
        ChargingSession {
            session_id,
            transaction_id: transaction_id.unwrap_or(session_id),
            bms,
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

    pub fn bms(&self) -> &Bms {
        &self.bms
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
        db: &Database,
        mut snapshot: ChargingSessionSnapshot,
    ) -> SoCProgress {
        if self.bms.initial_energy.is_none() {
            // TODO check this is real world situations before turning it into an assertion?
            error!("FIXME unknown BMS ref energy while adding a non-reference snapshot");
        }

        self.bms.have_energy(snapshot.energy);
        snapshot.soc = self.bms.current_soc;

        db.add_charging_session_snapshot(self.session_id, &snapshot);
        self.snapshots.push(snapshot);

        self.bms.soc_progress()
    }

    pub fn add_snapshot(&mut self, snapshot: ChargingSessionSnapshot) -> SoCProgress {
        self.add_and_save_snapshot(&Database::get(), snapshot)
    }

    pub fn add_snapshot_from_database(&mut self, snapshot: ChargingSessionSnapshot) {
        self.snapshots.push(snapshot);
    }

    /// Performs updates to this retrieved `ChargingSession`
    ///
    /// Updates include:
    ///
    /// * setting BMS initial energy
    /// * setting BMS initial SoC (if user hasn't apply any corrections)
    /// * setting BMS current SoC (if user hasn't apply any corrections)
    /// * adding a new reference snapshot if user has applied a SoC correction
    ///
    /// Returns the new reference snapshot if added.
    pub fn finalise_last_session_retrieval(&mut self) -> Option<&ChargingSessionSnapshot> {
        if self.state.is_complete() {
            return None;
        }

        // EV has been charging on the exact same session
        // check if we need to update SoC according to user indications

        let retrieved_initial_soc = self.initial_soc();

        let Some(first_snapshot) = self.snapshots.first() else {
            error!("FIXME: session: {}, no first snapshot", self.session_id);
            return None;
        };
        let Some(ref_snapshot) = self.last_reference_snapshot() else {
            error!(
                "FIXME: session: {}, no reference snapshots",
                self.session_id
            );
            return None;
        };

        let first_energy = first_snapshot.energy;
        let ref_snapshot_cap = ref_snapshot.soc_cap;
        let last_known_energy = self.snapshots.last().expect("at least one").energy;

        self.bms.initial_energy = Some(first_energy as f64);
        self.bms.current_energy = Some(last_known_energy as f64);

        if self.bms.soc_cap.is_none() && ref_snapshot_cap.is_some() {
            warn!(
                "removed SoC cap for a recovered uncomplete session, \
                    make sure this is really what you intended to do"
            );
        }

        let mut add_reference_snapshot = false;
        match (retrieved_initial_soc, self.bms.initial_soc) {
            (retrieved_init_soc, SoC::Unknown) => {
                self.bms.initial_soc.update(retrieved_init_soc);
            }
            (retrieved_init_soc, bms_init_soc) if retrieved_init_soc != bms_init_soc => {
                warn!(
                    "specified different initial SoC for a recovered uncomplete session, \
                            make sure this is what you intended to do"
                );
                add_reference_snapshot = true;
            }
            (SoC::Unknown, bms_init_soc) if !bms_init_soc.is_unknown() => {
                warn!(
                    "specified initial SoC {bms_init_soc} for a recovered uncomplete session \
                        for which the initial SoC was previously unknown, \
                        make sure this is what you intended to do"
                );
                add_reference_snapshot = true;
            }
            _ => (),
        }

        let last_snapshot = self.last_snapshot().expect("at least one snapshot");

        if add_reference_snapshot {
            let first_energy = first_energy as f64;
            let last_snapshot_energy = last_snapshot.energy as f64;
            if first_energy <= last_snapshot_energy {
                let current_soc = self.bms.initial_soc
                    + (last_snapshot_energy - first_energy) / self.bms.capacity;

                let new_snapshot =
                    ChargingSessionSnapshot::builder(Utc::now(), last_snapshot.energy)
                        .is_reference(true)
                        .soc(current_soc)
                        .build();

                self.snapshots.push(new_snapshot);
                self.bms.current_soc = current_soc;

                return Some(self.snapshots.last().unwrap());
            } else {
                error!(
                    "FIXME: session: {}, first snapshot energy: {:.3} > last snapshot energy: {:.3}",
                    self.session_id,
                    first_energy / 1_000.0,
                    last_snapshot_energy / 1_000.0,
                );
            }
        }

        self.bms.current_soc = last_snapshot.soc;

        None
    }

    pub fn stop(
        &mut self,
        timestamp: DateTime<Utc>,
        energy: u64,
        reason: impl Into<ChargingSessionState>,
    ) {
        let db = Database::get();

        self.add_and_save_snapshot(&db, ChargingSessionSnapshot::new(timestamp, energy));
        let reason = reason.into();
        db.stop_charging_session(self.session_id, &reason);
        self.state = reason;
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
        let soc_delta = (first_to_ref_energy as f64) / self.bms.capacity;

        ref_snapshot.soc - soc_delta
    }
}

impl fmt::Display for ChargingSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "sid: {}, state: {}, last SoC: {}, initial SoC: {}, tid: {:?}",
            self.session_id,
            self.state,
            self.last_soc(),
            self.initial_soc(),
            self.transaction_id,
        ))
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub enum ChargingSessionState {
    Active,
    SocCapReached,
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
        // FIXME unsure about:
        // * SuspendedByEv (probably same as SuspendedByEvse)
        matches!(
            self,
            SocCapReached | StoppedByUser | Reboot | UnlockCommandFromServer | Error(_)
        )
    }
}

impl fmt::Display for ChargingSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ChargingSessionState::*;
        f.write_str(match self {
            Active => "Active",
            SocCapReached => "SocCapReached",
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
            "Active" => Active,
            "SocCapReached" => SocCapReached,
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
            Charging | Preparing => Active,
            SuspendedEVSE => SuspendedByEvse,
            SuspendedEV => SuspendedByEv,
            Finishing => SocCapReached,
            Unavailable => Error("Unavailable".to_string()),
            Faulted => Error("Faulted".to_string()),
            Available => panic!("should not be called in this state"),
            Reserved => unimplemented!(),
        }
    }
}

impl From<Option<enums::Reason>> for ChargingSessionState {
    fn from(reason: Option<enums::Reason>) -> Self {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BATTERY_CAPACITY: u32 = 48_100;
    const CONST_POWER_LOSS: u16 = 400;
    const INITIAL_ENERGY: u64 = 100;
    const MAX_ADDED_ENERGY_FIRST_PERIOD: u64 = 25;

    #[test]
    #[ignore = "backup the database before running this test"]
    fn charging_session() {
        crate::tests::init();

        // run the tests sequentially so get_last_active_charging_session
        // returns what it is supposed to return
        session_with_soc_and_soc_cap();
        session_unknown_initial_soc();
        session_unknown_initial_soc_no_cap();
        multi_period_session_with_soc_and_soc_cap();
        multi_period_session_unknown_initial_soc();
        missed_start_active_session_with_soc_and_soc_cap();
        missed_start_suspended_session_with_soc_and_soc_cap();
    }

    fn session_with_soc_and_soc_cap() {
        let start_time = Utc::now();
        let initial_soc = 0.3f64;
        let soc_cap = initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(initial_soc))
            .soc_cap(soc_cap)
            .build();
        let mut cs = ChargingSession::new(bms, start_time, INITIAL_ENERGY);
        assert_eq!(initial_soc, cs.bms().initial_soc.absolute().unwrap());
        assert_eq!(Some(soc_cap), cs.bms().soc_cap);
        let last_snapshot = cs.last_snapshot().unwrap();
        assert_eq!(last_snapshot.timestamp, start_time);
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.energy, INITIAL_ENERGY);

        let snapshot_ts = Utc::now();
        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(snapshot_ts, INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteCapNotReached {
                soc: initial_soc + 10f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
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
            ChargingSessionSnapshot::builder(snapshot_ts, INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteCapNotReached {
                soc: initial_soc + 20f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
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

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(&ChargingSessionState::Active, last_session.unwrap().state());

        let soc_progress = cs.add_snapshot(
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
            SoCProgress::AbsoluteCapReached {
                soc: initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
        cs.stop(
            Utc::now(),
            INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            ChargingSessionState::SocCapReached,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SocCapReached,
            last_session.unwrap().state()
        );
    }

    fn session_unknown_initial_soc() {
        let start_time = Utc::now();
        let soc_cap = MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(soc_cap)
            .build();
        let mut cs = ChargingSession::new(bms.clone(), start_time, INITIAL_ENERGY);
        assert_eq!(SoC::Unknown, cs.bms().initial_soc);
        assert_eq!(Some(soc_cap), cs.bms().soc_cap);
        assert!(cs.last_snapshot().unwrap().is_reference);

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::RelativeCapNotReached {
                rel_soc: 10f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::RelativeCapNotReached {
                rel_soc: 20f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get().get_last_charging_session(&bms).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(&ChargingSessionState::Active, last_session.unwrap().state());

        let soc_progress = cs.add_snapshot(
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
                rel_soc: MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
        cs.stop(
            Utc::now(),
            INITIAL_ENERGY + MAX_ADDED_ENERGY_FIRST_PERIOD,
            ChargingSessionState::SocCapReached,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SocCapReached,
            last_session.unwrap().state()
        );
    }

    fn session_unknown_initial_soc_no_cap() {
        let start_time = Utc::now();

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS).build();
        let mut cs = ChargingSession::new(bms, start_time, INITIAL_ENERGY);
        assert_eq!(SoC::Unknown, cs.bms().initial_soc);
        assert!(cs.bms().soc_cap.is_none());
        assert!(cs.last_snapshot().unwrap().is_reference);

        assert_eq!(SoC::Unknown, cs.bms().initial_soc);
        assert_eq!(
            SoCProgress::RelativeUncapped(10f64 / BATTERY_CAPACITY as f64),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert_eq!(
            SoCProgress::RelativeUncapped(20f64 / BATTERY_CAPACITY as f64),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(&ChargingSessionState::Active, last_session.unwrap().state());

        assert_eq!(
            Some(&cs),
            Database::get()
                .get_last_charging_session(cs.bms())
                .unwrap()
                .as_ref(),
        );

        cs.stop(
            Utc::now(),
            INITIAL_ENERGY + 25,
            ChargingSessionState::SuspendedByEv,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SuspendedByEv,
            last_session.unwrap().state()
        );
    }

    fn multi_period_session_with_soc_and_soc_cap() {
        let start_time = Utc::now();
        let initial_soc = 0.3f64;
        let mut soc_cap =
            initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(initial_soc))
            .soc_cap(soc_cap)
            .build();
        let mut cs = ChargingSession::new(bms, start_time, INITIAL_ENERGY);
        assert!(cs.last_snapshot().unwrap().is_reference);

        let _ = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        let _ = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        // Simulate server going offline, then reconnecting without the session being suspended
        // not changing initial SoC and expending the SoC cap
        soc_cap += 10f64 / BATTERY_CAPACITY as f64;
        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(soc_cap)
            .build();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(
            Some(INITIAL_ENERGY as f64),
            recovered_session.bms().initial_energy,
        );
        assert_eq!(
            SoC::Absolute(initial_soc),
            recovered_session.bms().initial_soc,
        );
        assert_eq!(Some(soc_cap), recovered_session.bms().soc_cap);

        let soc_progress = recovered_session.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 30)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert_eq!(
            SoCProgress::AbsoluteCapNotReached {
                soc: initial_soc + 30f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        // Simulate server going offline again,
        // then reconnecting without the session being suspended
        // this time, correcting initial SoC and removing the SoC cap
        let corrected_initial_soc = initial_soc - 10f64 / BATTERY_CAPACITY as f64;
        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(corrected_initial_soc))
            .build();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(
            Some(INITIAL_ENERGY as f64),
            recovered_session.bms().initial_energy,
        );
        assert_eq!(
            SoC::Absolute(corrected_initial_soc),
            recovered_session.bms().initial_soc,
        );
        assert!(recovered_session.bms().soc_cap.is_none());
        let soc_progress = recovered_session.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 40)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert_eq!(
            SoCProgress::AbsoluteUncapped(corrected_initial_soc + 40f64 / BATTERY_CAPACITY as f64),
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);
    }

    fn multi_period_session_unknown_initial_soc() {
        let start_time = Utc::now();
        let mut soc_cap = MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(soc_cap)
            .build();
        let mut cs = ChargingSession::new(bms, start_time, INITIAL_ENERGY);
        assert!(cs.last_snapshot().unwrap().is_reference);

        let _ = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        let _ = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        // Simulate server going offline, then reconnecting without the session being suspended
        // not changing initial SoC and expending the SoC cap
        soc_cap += 10f64 / BATTERY_CAPACITY as f64;
        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(soc_cap)
            .build();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(
            recovered_session.bms().initial_energy,
            Some(INITIAL_ENERGY as f64)
        );
        // recovered session started from an unknown reference SoC
        assert_eq!(SoC::Unknown, recovered_session.bms().initial_soc,);
        assert_eq!(Some(soc_cap), recovered_session.bms().soc_cap);

        let soc_progress = recovered_session.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 30)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert_eq!(
            SoCProgress::RelativeCapNotReached {
                rel_soc: 30f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        // Simulate server going offline again,
        // then reconnecting without the session being suspended
        // this time, correcting initial SoC and removing the SoC cap
        let corrected_initial_soc = 0.30;
        let corrected_ref_soc = corrected_initial_soc + 30.0 / BATTERY_CAPACITY as f64;
        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(corrected_initial_soc))
            .build();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(
            Some(INITIAL_ENERGY as f64),
            recovered_session.bms().initial_energy,
        );
        assert_eq!(
            SoC::Absolute(corrected_initial_soc),
            recovered_session.bms().initial_soc,
        );
        assert_eq!(None, recovered_session.bms().soc_cap,);
        let last_snapshot = recovered_session.last_snapshot().unwrap();
        assert!(last_snapshot.is_reference);
        assert_eq!(SoC::Absolute(corrected_ref_soc), last_snapshot.soc);
        assert_eq!(None, last_snapshot.soc_cap);

        let soc_progress = recovered_session.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 40)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        let last_snapshot = recovered_session.last_snapshot().unwrap();
        let mut expected_soc = corrected_ref_soc + 10f64 / recovered_session.bms().capacity;
        match soc_progress {
            SoCProgress::AbsoluteUncapped(cur_soc) => {
                assert!((expected_soc - cur_soc).abs() < 0.00000000001);
            }
            other => unreachable!("{other:?}"),
        }
        assert!(!last_snapshot.is_reference);

        // Simulate server going offline again,
        // then reconnecting without the session being suspended
        // this time, using unknown SoC and SoC cap
        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS).build();
        let mut recovered_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(
            recovered_session.bms().initial_energy,
            Some(INITIAL_ENERGY as f64)
        );
        assert_eq!(
            SoC::Absolute(corrected_initial_soc),
            recovered_session.bms().initial_soc
        );
        let soc_progress = recovered_session.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 50)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        expected_soc += 10f64 / recovered_session.bms().capacity;
        match soc_progress {
            SoCProgress::AbsoluteUncapped(cur_soc) => {
                assert!((expected_soc - cur_soc).abs() < 0.00000000001);
            }
            other => unreachable!("{other:?}"),
        }
        assert!(!cs.last_snapshot().unwrap().is_reference);
    }

    /// Simulates receiving a MeterValue with active enery import
    fn missed_start_active_session_with_soc_and_soc_cap() {
        let initial_soc = 0.3f64;
        let soc_cap = initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(initial_soc))
            .soc_cap(soc_cap)
            .build();

        let current_time = Utc::now();
        let last_known_energy = INITIAL_ENERGY;
        let tid = 1;
        let mut cs = ChargingSession::with_state(
            bms.clone(),
            tid,
            ChargingSessionState::Unknown,
            last_known_energy,
            current_time,
        );
        assert_eq!(cs.state, ChargingSessionState::Unknown);
        assert_eq!(cs.transaction_id, tid);

        let last_snapshot = cs.last_snapshot().unwrap();
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.timestamp, current_time);
        assert_eq!(last_snapshot.energy, last_known_energy);

        assert_eq!(Some(last_known_energy as f64), cs.bms().initial_energy);
        assert_eq!(SoC::Absolute(initial_soc), cs.bms().initial_soc);
        assert_eq!(Some(soc_cap), cs.bms().soc_cap);

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(soc_cap)
            .build();
        let last_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);

        cs.set_state(ChargingSessionState::Active);
        assert_eq!(cs.state, ChargingSessionState::Active);

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteCapNotReached {
                soc: initial_soc + 10f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);
    }

    fn missed_start_suspended_session_with_soc_and_soc_cap() {
        let initial_soc = 0.3f64;
        let soc_cap = initial_soc + MAX_ADDED_ENERGY_FIRST_PERIOD as f64 / BATTERY_CAPACITY as f64;

        let bms = Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(initial_soc))
            .soc_cap(soc_cap)
            .build();

        let current_time = Utc::now();
        let current_energy = INITIAL_ENERGY;
        let tid = 1;
        let mut cs = ChargingSession::with_state(
            bms.clone(),
            tid,
            enums::ChargePointStatus::SuspendedEVSE,
            current_energy,
            current_time,
        );
        assert_eq!(cs.state, ChargingSessionState::SuspendedByEvse);
        assert_eq!(cs.transaction_id, tid);

        let last_snapshot = cs.last_snapshot().unwrap();
        assert!(last_snapshot.is_reference);
        assert_eq!(last_snapshot.timestamp, current_time);
        assert_eq!(last_snapshot.energy, current_energy);

        let cs_bms = cs.bms();
        assert_eq!(Some(current_energy as f64), cs_bms.initial_energy);
        assert_eq!(SoC::Absolute(initial_soc), cs_bms.initial_soc);
        assert_eq!(Some(soc_cap), cs_bms.soc_cap);

        let last_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);

        cs.set_state(ChargingSessionState::Active);
        assert_eq!(cs.state, ChargingSessionState::Active);

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), INITIAL_ENERGY + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SoCProgress::AbsoluteCapNotReached {
                soc: initial_soc + 10f64 / BATTERY_CAPACITY as f64,
                cap: soc_cap,
            },
            soc_progress,
        );
        assert!(!cs.last_snapshot().unwrap().is_reference);

        let last_session = Database::get()
            .get_last_charging_session(&bms)
            .expect("can retrieve last session")
            .expect("last session exists");
        assert_eq!(cs, last_session);
    }
}
