use chrono::{DateTime, Local, Utc};

use crate::Database;

pub const ACTIVE_CHARGING_SESSION_STATE: &str = "ACTIVE";
const BATTERY_CAPACITY: f64 = 48_100.0;

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
    initial_soc: Option<f64>,
    snapshots: Vec<ChargingSessionSnapshot>,
    stop_reason: String,
}

impl ChargingSession {
    pub fn new(timestamp: DateTime<Utc>, energy: u64, soc: Option<f64>) -> Self {
        let db = Database::get();

        let session_id = db.add_new_charging_session();

        let mut this = ChargingSession {
            session_id,
            initial_energy: energy,
            initial_soc: soc,
            snapshots: vec![],
            stop_reason: ACTIVE_CHARGING_SESSION_STATE.to_string(),
        };
        this.add_snapshot_priv(&db, timestamp, energy, 0);

        this
    }

    pub fn from_database(session_id: i32, energy: u64, soc: Option<f64>, state: &str) -> Self {
        ChargingSession {
            session_id,
            initial_energy: energy,
            initial_soc: soc,
            snapshots: vec![],
            // FIXME mismatch
            stop_reason: state.to_string(),
        }
    }

    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    pub fn add_snapshot(
        &mut self,
        timestamp: DateTime<Utc>,
        energy: u64,
        power: u64,
    ) -> Option<f64> {
        self.add_snapshot_priv(&Database::get(), timestamp, energy, power)
    }

    fn add_snapshot_priv(
        &mut self,
        db: &Database,
        timestamp: DateTime<Utc>,
        energy: u64,
        power: u64,
    ) -> Option<f64> {
        let energy_delta = energy.saturating_sub(self.initial_energy);
        let soc = self.initial_soc.map(|initial_soc| {
            let soc = energy_delta as f64 / BATTERY_CAPACITY + initial_soc;
            log::info!(
                "## session: {}, SoC: {soc}, energy {} kWh",
                self.session_id,
                energy_delta as f64 / 1_000f64
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

        soc
    }

    pub fn add_snapshot_from_database(
        &mut self,
        timestamp: &str,
        energy: u64,
        power: u64,
        soc: Option<f64>,
    ) {
        self.snapshots.push(ChargingSessionSnapshot {
            timestamp: timestamp
                .parse::<DateTime<Utc>>()
                .unwrap()
                .with_timezone(&Local),
            energy,
            power,
            soc,
        });
    }

    pub fn stop(&mut self, timestamp: DateTime<Utc>, energy: u64, reason: impl ToString) {
        let db = Database::get();

        self.add_snapshot_priv(&db, timestamp, energy, 0);
        let reason = reason.to_string();

        db.stop_charging_session(self.session_id, &reason);
        self.stop_reason = reason;
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
    fn regular_session() {
        init();

        let start_time = Utc::now();
        let initial_energy = 100;
        let initial_soc = 30.0f64;

        let mut cs = ChargingSession::new(start_time, initial_energy, Some(initial_soc));
        println!("inserted charging session with id: {}", cs.session_id());

        cs.add_snapshot(Utc::now(), initial_energy + 10, 10);
        cs.add_snapshot(Utc::now(), initial_energy + 20, 10);
        assert_eq!(
            Database::get().get_last_active_charging_session().as_ref(),
            Some(&cs),
        );

        cs.stop(Utc::now(), initial_energy + 25, "SuspendedEV");
        assert_eq!(
            Database::get().get_last_active_charging_session().as_ref(),
            None,
        );
        println!("charging session: {cs:?}");
    }
}
