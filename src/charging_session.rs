use chrono::{DateTime, Local, Utc};
use ocpp_rs::v16::enums;
use std::{fmt, str};

use crate::{Bms, Database};

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Local>,
    pub energy: u64,
    pub power: u64,
    pub soc: Option<f64>,
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
    pub fn new(timestamp: DateTime<Utc>, energy: u64, bms: Bms) -> Self {
        let db = Database::get();

        let session_id = db.add_new_charging_session(bms);

        let mut this = ChargingSession {
            session_id,
            initial_energy: energy,
            bms,
            snapshots: vec![],
            state: ChargingSessionState::Active,
        };
        this.add_snapshot_priv(&db, timestamp, energy, 0);

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

    pub fn add_snapshot(
        &mut self,
        timestamp: DateTime<Utc>,
        energy: u64,
        power: u64,
    ) -> SocProgress {
        self.add_snapshot_priv(&Database::get(), timestamp, energy, power)
    }

    fn add_snapshot_priv(
        &mut self,
        db: &Database,
        timestamp: DateTime<Utc>,
        energy: u64,
        power: u64,
    ) -> SocProgress {
        let energy_delta = energy.saturating_sub(self.initial_energy);
        let soc = self.bms.initial_soc.map(|initial_soc| {
            let soc = energy_delta as f64 / self.bms.capacity + initial_soc;
            log::info!(
                "## session: {}, SoC: {soc}, energy {} kWh",
                self.session_id,
                energy_delta as f64 / 1_000.0
            );

            soc
        });

        let snapshot = ChargingSessionSnapshot {
            timestamp: timestamp.with_timezone(&Local),
            energy,
            power,
            soc,
        };

        db.add_charging_session_snapshot(self.session_id, &snapshot);
        self.snapshots.push(snapshot);

        let Some(soc) = soc else {
            return SocProgress::Unknown;
        };

        let Some(soc_cap) = self.bms.soc_cap else {
            return SocProgress::Uncapped(soc);
        };

        if soc >= soc_cap {
            return SocProgress::CapReached { soc, cap: soc_cap };
        }

        SocProgress::CapNotReached { soc, cap: soc_cap }
    }

    pub fn add_snapshot_from_database(
        &mut self,
        timestamp: DateTime<Local>,
        energy: u64,
        power: u64,
        soc: Option<f64>,
    ) {
        self.snapshots.push(ChargingSessionSnapshot {
            timestamp,
            energy,
            power,
            soc,
        });
    }

    pub fn stop(
        &mut self,
        timestamp: DateTime<Utc>,
        energy: u64,
        reason: impl Into<ChargingSessionState>,
    ) {
        let db = Database::get();

        self.add_snapshot_priv(&db, timestamp, energy, 0);
        let reason = reason.into();
        db.stop_charging_session(self.session_id, &reason);
        self.state = reason;
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum SocProgress {
    Unknown,
    Uncapped(f64),
    CapNotReached { soc: f64, cap: f64 },
    CapReached { soc: f64, cap: f64 },
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
    }

    fn session_regular() {
        let start_time = Utc::now();
        let initial_energy = 100;
        let battery_capacity = 48_100f64;
        let initial_soc = 0.3f64;
        let max_added_energy = 25u64;
        let soc_cap = initial_soc + max_added_energy as f64 / battery_capacity;

        let battery = Bms {
            capacity: battery_capacity,
            initial_soc: Some(initial_soc),
            soc_cap: Some(soc_cap),
        };
        let mut cs = ChargingSession::new(start_time, initial_energy, battery);
        println!("inserted charging session with id: {}", cs.session_id());

        assert_eq!(
            cs.add_snapshot(Utc::now(), initial_energy + 10, 10),
            SocProgress::CapNotReached {
                soc: battery.initial_soc.unwrap() + 10f64 / battery.capacity,
                cap: soc_cap,
            },
        );
        assert_eq!(
            cs.add_snapshot(Utc::now(), initial_energy + 20, 10),
            SocProgress::CapNotReached {
                soc: battery.initial_soc.unwrap() + 20f64 / battery.capacity,
                cap: soc_cap,
            },
        );
        assert_eq!(
            Database::get()
                .get_last_active_charging_session()
                .unwrap()
                .as_ref(),
            Some(&cs),
        );

        assert_eq!(
            cs.add_snapshot(Utc::now(), initial_energy + max_added_energy, 10),
            SocProgress::CapReached {
                soc: battery.initial_soc.unwrap() + max_added_energy as f64 / battery.capacity,
                cap: soc_cap,
            },
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

        let mut cs = ChargingSession::new(
            start_time,
            initial_energy,
            Bms {
                capacity: 48_100.0,
                initial_soc: None,
                soc_cap: None,
            },
        );
        println!("inserted charging session with id: {}", cs.session_id());

        assert_eq!(
            cs.add_snapshot(Utc::now(), initial_energy + 10, 10),
            SocProgress::Unknown
        );
        assert_eq!(
            cs.add_snapshot(Utc::now(), initial_energy + 20, 10),
            SocProgress::Unknown
        );
        assert_eq!(
            Database::get()
                .get_last_active_charging_session()
                .unwrap()
                .as_ref(),
            Some(&cs),
        );

        cs.stop(
            Utc::now(),
            initial_energy + 25,
            ChargingSessionState::SuspendedByEv,
        );
        assert_eq!(
            Database::get().get_last_active_charging_session().unwrap(),
            None,
        );
        println!("charging session: {cs:?}");
    }
}
