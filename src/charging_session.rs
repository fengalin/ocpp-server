use chrono::{DateTime, Utc};
use ocpp_rs::v16::enums;
use std::{fmt, str};

use crate::{Bms, Database, SocProgress};

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
    pub soc: Option<f64>,
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
            soc: None,
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
    pub fn soc(mut self, soc: impl Into<Option<f64>>) -> Self {
        self.0.soc = soc.into();
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
    bms: Bms,
    snapshots: Vec<ChargingSessionSnapshot>,
    state: ChargingSessionState,
}

impl ChargingSession {
    pub fn new(bms: Bms, timestamp: DateTime<Utc>, energy: u64) -> Self {
        let db = Database::get();

        let session_id = db.add_new_charging_session(ChargingSessionState::Active, None);

        let mut this = ChargingSession {
            session_id,
            bms,
            snapshots: vec![],
            state: ChargingSessionState::Active,
        };
        let snapshot = ChargingSessionSnapshot::builder(timestamp, energy)
            .is_reference(true)
            .soc(this.bms.reference_soc)
            .soc_cap(this.bms.soc_cap)
            .build();
        this.add_and_save_snapshot(&db, snapshot);

        this
    }

    /// Builds a ChargingSession in the specified state
    ///
    /// This is useful for cases where we missed the transaction start.
    pub fn with_state(
        state: impl Into<ChargingSessionState>,
        initial_energy: u64,
        bms: Bms,
        transaction_id: impl Into<Option<i32>>,
        timestamp: impl Into<Option<DateTime<Utc>>>,
    ) -> Self {
        let db = Database::get();

        let state = state.into();
        let session_id = db.add_new_charging_session(state.clone(), transaction_id.into());

        let snapshot = ChargingSessionSnapshot::builder(
            timestamp.into().unwrap_or_else(Utc::now),
            initial_energy,
        )
        .is_reference(true)
        .soc(bms.reference_soc)
        .soc_cap(bms.soc_cap)
        .build();

        let mut this = ChargingSession {
            session_id,
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

    pub fn from_database(session_id: i32, bms: Bms, state: ChargingSessionState) -> Self {
        ChargingSession {
            session_id,
            bms,
            snapshots: vec![],
            state,
        }
    }

    pub fn session_id(&self) -> i32 {
        self.session_id
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

    fn update_energy_and_soc(&mut self, snapshot: &mut ChargingSessionSnapshot) -> SocProgress {
        if snapshot.is_reference {
            self.bms.set_reference(snapshot.energy);
        } else if self.bms.reference_energy.is_none() {
            // if this is the first snapshot after retrieving from db
            // use this snapshot as a reference
            snapshot.is_reference = true;
            self.bms.set_reference(snapshot.energy);
        }

        let soc_progress = self.bms.get_current_soc(snapshot.energy);
        snapshot.soc = soc_progress.soc();
        if snapshot.soc_cap.is_none() {
            snapshot.soc_cap = self.bms.soc_cap;
        }

        soc_progress
    }

    fn add_and_save_snapshot(
        &mut self,
        db: &Database,
        mut snapshot: ChargingSessionSnapshot,
    ) -> SocProgress {
        let soc_progress = self.update_energy_and_soc(&mut snapshot);

        db.add_charging_session_snapshot(self.session_id, &snapshot);
        self.snapshots.push(snapshot);

        soc_progress
    }

    pub fn add_snapshot(&mut self, snapshot: ChargingSessionSnapshot) -> SocProgress {
        self.add_and_save_snapshot(&Database::get(), snapshot)
    }

    pub fn add_snapshot_from_database(&mut self, snapshot: ChargingSessionSnapshot) {
        self.snapshots.push(snapshot);
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

    pub fn last_soc(&self) -> Option<f64> {
        self.last_snapshot()
            .expect("at least one snapshot at this stage")
            .soc
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
                return write!(f, "Error: {err}");
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

    fn init() {
        use std::sync::Once;
        static LOGGER: Once = Once::new();
        LOGGER.call_once(|| {
            env_logger::builder()
                .format_source_path(true)
                .format_line_number(true)
                .try_init()
                .unwrap();
        });
    }

    #[test]
    #[ignore = "backup the database before running this test"]
    fn charging_session() {
        init();

        // run the tests sequentially so get_last_active_charging_session
        // returns what it is supposed to return
        session_regular();
        session_unknown_initial_soc();
        session_unknown_initial_soc_no_cap();
    }

    fn session_regular() {
        let start_time = Utc::now();
        let initial_energy = 100;
        let battery_capacity = 48_100f64;
        let initial_soc = 0.3f64;
        let max_added_energy = 25u64;
        let soc_cap = initial_soc + max_added_energy as f64 / battery_capacity;

        let bms = Bms {
            capacity: battery_capacity,
            constant_power_loss: 400,
            reference_energy: None,
            reference_soc: Some(initial_soc),
            soc_cap: Some(soc_cap),
        };
        let mut cs = ChargingSession::new(bms, start_time, initial_energy);
        println!("inserted charging session with id: {}", cs.session_id());

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SocProgress::AbsoluteCapNotReached {
                soc: cs.bms().reference_soc.unwrap() + 10f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SocProgress::AbsoluteCapNotReached {
                soc: cs.bms().reference_soc.unwrap() + 20f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(&ChargingSessionState::Active, last_session.unwrap().state());

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + max_added_energy)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert!(soc_progress.is_complete());
        assert_eq!(
            SocProgress::AbsoluteCapReached {
                soc: cs.bms().reference_soc.unwrap() + max_added_energy as f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        cs.stop(
            Utc::now(),
            initial_energy + max_added_energy,
            ChargingSessionState::SocCapReached,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SocCapReached,
            last_session.unwrap().state()
        );

        println!("charging session: {cs:?}");
    }

    fn session_unknown_initial_soc() {
        let start_time = Utc::now();
        let initial_energy = 100;
        let battery_capacity = 48_100f64;
        let max_added_energy = 25u64;
        let soc_cap = max_added_energy as f64 / battery_capacity;

        let bms = Bms {
            capacity: battery_capacity,
            constant_power_loss: 400,
            reference_energy: None,
            reference_soc: None,
            soc_cap: Some(soc_cap),
        };
        let mut cs = ChargingSession::new(bms, start_time, initial_energy);
        println!("inserted charging session with id: {}", cs.session_id());

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 10)
                .power(10)
                .l1_voltage(230)
                .temperature(27)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SocProgress::RelativeCapNotReached {
                added_soc: 10f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 20)
                .power(10)
                .l1_voltage(228)
                .temperature(28)
                .build(),
        );
        assert!(!soc_progress.is_complete());
        assert_eq!(
            SocProgress::RelativeCapNotReached {
                added_soc: 20f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(&ChargingSessionState::Active, last_session.unwrap().state());

        let soc_progress = cs.add_snapshot(
            ChargingSessionSnapshot::builder(Utc::now(), initial_energy + max_added_energy)
                .power(10)
                .l1_voltage(227)
                .temperature(30)
                .build(),
        );
        assert!(soc_progress.is_complete());
        assert_eq!(
            SocProgress::RelativeCapReached {
                added_soc: max_added_energy as f64 / cs.bms().capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        cs.stop(
            Utc::now(),
            initial_energy + max_added_energy,
            ChargingSessionState::SocCapReached,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SocCapReached,
            last_session.unwrap().state()
        );

        println!("charging session: {cs:?}");
    }

    fn session_unknown_initial_soc_no_cap() {
        let start_time = Utc::now();
        let initial_energy = 100;

        let bms = Bms {
            capacity: 48_100.0,
            constant_power_loss: 400,
            reference_energy: None,
            reference_soc: None,
            soc_cap: None,
        };
        let mut cs = ChargingSession::new(bms, start_time, initial_energy);
        println!("inserted charging session with id: {}", cs.session_id());

        assert_eq!(
            SocProgress::RelativeUncapped(10f64 / cs.bms().capacity),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 10)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert_eq!(
            SocProgress::RelativeUncapped(20f64 / cs.bms().capacity),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 20)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );

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
            initial_energy + 25,
            ChargingSessionState::SuspendedByEv,
        );

        let last_session = Database::get().get_last_charging_session(cs.bms()).unwrap();
        assert_eq!(Some(&cs), last_session.as_ref());
        assert_eq!(
            &ChargingSessionState::SuspendedByEv,
            last_session.unwrap().state()
        );

        println!("charging session: {cs:?}");
    }
}
