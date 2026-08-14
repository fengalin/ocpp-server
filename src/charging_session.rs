use chrono::{DateTime, Utc};
use ocpp_rs::v16::enums;
use std::{fmt, str};

use crate::{Bms, Database, SocProgress};

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Utc>,
    pub energy: u64,
    pub power: Option<u64>,
    pub l1_voltage: Option<u64>,
    pub temperature: Option<u64>,
    pub soc: Option<f64>,
}
impl ChargingSessionSnapshot {
    pub fn new(timestamp: DateTime<Utc>, energy: u64) -> Self {
        ChargingSessionSnapshot {
            timestamp,
            energy,
            power: None,
            l1_voltage: None,
            temperature: None,
            soc: None,
        }
    }

    pub fn builder(timestamp: DateTime<Utc>, energy: u64) -> ChargingSessionSnapshotBuilder {
        ChargingSessionSnapshotBuilder(Self::new(timestamp, energy))
    }
}
pub struct ChargingSessionSnapshotBuilder(ChargingSessionSnapshot);
impl ChargingSessionSnapshotBuilder {
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
    pub fn build(self) -> ChargingSessionSnapshot {
        self.0
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSession {
    session_id: i32,
    initial_energy: u64,
    bms: Bms,
    snapshots: Vec<ChargingSessionSnapshot>,
    state: ChargingSessionState,
}

impl ChargingSession {
    pub fn new(bms: Bms, timestamp: DateTime<Utc>, energy: u64) -> Self {
        let db = Database::get();

        let session_id = db.add_new_charging_session(bms);

        let mut this = ChargingSession {
            session_id,
            initial_energy: energy,
            bms,
            snapshots: vec![],
            state: ChargingSessionState::Active,
        };
        let snapshot = ChargingSessionSnapshot::builder(timestamp, energy)
            .soc(bms.initial_soc)
            .build();
        db.add_charging_session_snapshot(this.session_id, &snapshot);
        this.snapshots.push(snapshot);

        this
    }

    pub fn from_database(
        session_id: i32,
        energy: u64,
        bms: Bms,
        state: ChargingSessionState,
    ) -> Self {
        ChargingSession {
            session_id,
            initial_energy: energy,
            bms,
            snapshots: vec![],
            state,
        }
    }

    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    pub fn state(&self) -> &ChargingSessionState {
        &self.state
    }

    pub fn set_state(&mut self, state: ChargingSessionState) {
        self.state = state;
    }

    pub fn add_snapshot(&mut self, snapshot: ChargingSessionSnapshot) -> SocProgress {
        self.add_snapshot_priv(&Database::get(), snapshot)
    }

    fn add_snapshot_priv(
        &mut self,
        db: &Database,
        mut snapshot: ChargingSessionSnapshot,
    ) -> SocProgress {
        let added_energy = snapshot.energy.saturating_sub(self.initial_energy);
        let soc_progress = self.bms.get_current_soc(added_energy);

        snapshot.soc = Some(soc_progress.soc());
        db.add_charging_session_snapshot(self.session_id, &snapshot);
        self.snapshots.push(snapshot);

        soc_progress
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

        self.add_snapshot_priv(&db, ChargingSessionSnapshot::new(timestamp, energy));
        let reason = reason.into();
        db.stop_charging_session(self.session_id, &reason);
        self.state = reason;
    }

    pub fn last_energy(&self) -> u64 {
        self.snapshots
            .last()
            .expect("at least one snapshot at this stage")
            .energy
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
    Error(String),
}

impl ChargingSessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, ChargingSessionState::Active)
    }
}

impl fmt::Display for ChargingSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ChargingSessionState::*;
        f.write_str(match self {
            Active => "active",
            SocCapReached => "SoC cap reached",
            SuspendedByEvse => "suspended by EVSE",
            SuspendedByEv => "suspended by EV",
            StoppedByUser => "stopped by User",
            Reboot => "reboot",
            UnlockCommandFromServer => "unlock command from server",
            Error(err) => {
                return write!(f, "error: {err}");
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
            "active" => Active,
            "SoC cap reached" => SocCapReached,
            "suspended by EVSE" => SuspendedByEvse,
            "suspended by EV" => SuspendedByEv,
            "stop by user" => StoppedByUser,
            "reboot" => Reboot,
            "unlock command from server" => UnlockCommandFromServer,
            other => Error(other.to_string()),
        })
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
        session_uknown_initial_soc();
        session_uknown_initial_soc_no_cap();
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
            initial_soc: Some(initial_soc),
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
            soc_progress,
            SocProgress::AbsoluteCapNotReached {
                soc: bms.initial_soc.unwrap() + 10f64 / bms.capacity,
                cap: soc_cap,
            },
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
                soc: bms.initial_soc.unwrap() + 20f64 / bms.capacity,
                cap: soc_cap,
            },
            soc_progress,
        );

        assert_eq!(
            Some(&cs),
            Database::get()
                .get_last_active_charging_session()
                .unwrap()
                .as_ref()
        );

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
                soc: bms.initial_soc.unwrap() + max_added_energy as f64 / bms.capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        cs.stop(
            Utc::now(),
            initial_energy + max_added_energy,
            ChargingSessionState::SocCapReached,
        );
        assert_eq!(
            Database::get().get_last_active_charging_session().unwrap(),
            None,
        );
        println!("charging session: {cs:?}");
    }

    fn session_uknown_initial_soc() {
        let start_time = Utc::now();
        let initial_energy = 100;
        let battery_capacity = 48_100f64;
        let max_added_energy = 25u64;
        let soc_cap = max_added_energy as f64 / battery_capacity;

        let bms = Bms {
            capacity: battery_capacity,
            constant_power_loss: 400,
            initial_soc: None,
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
            soc_progress,
            SocProgress::RelativeCapNotReached {
                added_soc: 10f64 / bms.capacity,
                cap: soc_cap,
            },
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
                added_soc: 20f64 / bms.capacity,
                cap: soc_cap,
            },
            soc_progress,
        );

        assert_eq!(
            Some(&cs),
            Database::get()
                .get_last_active_charging_session()
                .unwrap()
                .as_ref()
        );

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
                added_soc: max_added_energy as f64 / bms.capacity,
                cap: soc_cap,
            },
            soc_progress,
        );
        cs.stop(
            Utc::now(),
            initial_energy + max_added_energy,
            ChargingSessionState::SocCapReached,
        );
        assert_eq!(
            Database::get().get_last_active_charging_session().unwrap(),
            None,
        );
        println!("charging session: {cs:?}");
    }

    fn session_uknown_initial_soc_no_cap() {
        let start_time = Utc::now();
        let initial_energy = 100;

        let bms = Bms {
            capacity: 48_100.0,
            constant_power_loss: 400,
            initial_soc: None,
            soc_cap: None,
        };
        let mut cs = ChargingSession::new(bms, start_time, initial_energy);
        println!("inserted charging session with id: {}", cs.session_id());

        assert_eq!(
            SocProgress::RelativeUncapped(10f64 / bms.capacity),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 10)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert_eq!(
            SocProgress::RelativeUncapped(20f64 / bms.capacity),
            cs.add_snapshot(
                ChargingSessionSnapshot::builder(Utc::now(), initial_energy + 20)
                    .power(10)
                    .l1_voltage(230)
                    .temperature(27)
                    .build()
            ),
        );
        assert_eq!(
            Some(&cs),
            Database::get()
                .get_last_active_charging_session()
                .unwrap()
                .as_ref(),
        );

        cs.stop(
            Utc::now(),
            initial_energy + 25,
            ChargingSessionState::SuspendedByEv,
        );
        assert_eq!(
            None,
            Database::get().get_last_active_charging_session().unwrap(),
        );
        println!("charging session: {cs:?}");
    }
}
