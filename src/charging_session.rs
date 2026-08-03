use chrono::{DateTime, Local, Utc};
use std::{
    fs,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

const SQLITE_PATH: &str = "../charging_sessions.sqlite";
const ACTIVE_STATE: &str = "ACTIVE";
pub static DATABASE: LazyLock<Mutex<sqlite::Connection>> = LazyLock::new(|| {
    let existed = fs::exists(SQLITE_PATH).expect("valid path");
    let db = sqlite::open(SQLITE_PATH).expect("be able to create the db");

    if !existed {
        let query = "
            CREATE TABLE charging_session (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                state STRING
            );
            CREATE TABLE charging_session_snapshot (
                timestamp STRING,
                sessionid INTEGER,
                energy INTEGER,
                power INTEGER,
                soc FLOAT,
                FOREIGN KEY (sessionid) REFERENCES charging_session(id)
            );
        ";
        db.execute(query).unwrap();
    }

    Mutex::new(db)
});

const BATTERY_CAPACITY: f64 = 48_100.0;

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Local>,
    pub energy: u64,
    pub power: u64,
    pub soc: f64,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ChargingSession {
    session_id: i32,
    initial_energy: u64,
    initial_soc: f64,
    snapshots: Vec<ChargingSessionSnapshot>,
    stop_reason: String,
}

impl ChargingSession {
    pub fn new(timestamp: DateTime<Utc>, energy: u64, soc: Option<f64>) -> Self {
        let db = DATABASE.lock().unwrap();

        let mut statement = db
            .prepare(format!(
                "INSERT INTO charging_session (state) VALUES ('{ACTIVE_STATE}') RETURNING rowid;"
            ))
            .unwrap();
        match statement.next() {
            Ok(sqlite::State::Row) => (),
            other => {
                panic!("failed to insert charging session: {other:?}");
            }
        };
        let session_id = statement.read::<i64, _>("id").expect("charging session id");

        let mut this = ChargingSession {
            session_id: session_id as _,
            initial_energy: energy,
            initial_soc: soc.unwrap_or_default(),
            snapshots: vec![],
            stop_reason: "".to_string(),
        };
        this.add_snapshot_priv(&db, timestamp, energy, 0);

        this
    }

    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    pub fn get_last_active() -> Option<Self> {
        let db = DATABASE.lock().unwrap();

        let mut statement = db
            .prepare(format!(
                "SELECT * FROM charging_session_snapshot
                WHERE sessionid IN (
                    SELECT id FROM charging_session
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='charging_session'
                    )
                    AND state == '{ACTIVE_STATE}'
                )
            "
            ))
            .unwrap();

        let mut cs = None;
        while let Ok(sqlite::State::Row) = statement.next() {
            let timestamp = statement.read::<String, _>("timestamp").unwrap();
            let energy = statement.read::<i64, _>("energy").unwrap() as u64;
            let power = statement.read::<i64, _>("power").unwrap() as u64;
            let soc = statement.read::<f64, _>("soc").unwrap();

            let cs = cs.get_or_insert_with(|| {
                let session_id = statement.read::<i64, _>("sessionid").unwrap() as i32;
                log::info!(
                    "## found active session: {session_id}, timestamp: {}",
                    DateTime::<Utc>::from_str(&timestamp)
                        .unwrap()
                        .with_timezone(&Local),
                );

                ChargingSession {
                    session_id,
                    initial_energy: energy,
                    initial_soc: soc,
                    snapshots: vec![],
                    stop_reason: "".to_string(),
                }
            });

            cs.snapshots.push(ChargingSessionSnapshot {
                timestamp: DateTime::<Utc>::from_str(&timestamp)
                    .unwrap()
                    .with_timezone(&Local),
                energy,
                power,
                soc,
            });
        }

        cs
    }

    pub fn add_snapshot(&mut self, timestamp: DateTime<Utc>, energy: u64, power: u64) {
        let db = DATABASE.lock().unwrap();
        self.add_snapshot_priv(&db, timestamp, energy, power);
    }

    fn add_snapshot_priv(
        &mut self,
        db: &sqlite::Connection,
        timestamp: DateTime<Utc>,
        energy: u64,
        power: u64,
    ) {
        let energy_delta = energy.saturating_sub(self.initial_energy);
        let soc = if energy_delta > 0 {
            let soc = energy_delta as f64 / BATTERY_CAPACITY + self.initial_soc;
            log::info!(
                "## session: {}, SoC: {soc}, energy {} kWh",
                self.session_id,
                energy_delta as f64 / 1_000f64
            );

            soc
        } else {
            0.0
        };

        self.snapshots.push(ChargingSessionSnapshot {
            timestamp: timestamp.with_timezone(&Local),
            energy,
            power,
            soc,
        });

        let res = db.execute(format!(
            "
            INSERT INTO charging_session_snapshot
            (timestamp, sessionid, energy, power, soc)
            VALUES (
                '{timestamp}',
                {session_id},
                {energy},
                {power},
                {soc}
            );
        ",
            session_id = self.session_id,
        ));

        if let Err(err) = res {
            log::warn!("could not insert charging session snapshot: {err}");
        };
    }

    pub fn stop(&mut self, timestamp: DateTime<Utc>, energy: u64, reason: impl ToString) {
        let db = DATABASE.lock().unwrap();

        self.add_snapshot_priv(&db, timestamp, energy, 0);
        let reason = reason.to_string();

        let res = db.execute(format!(
            "
            UPDATE charging_session SET state = '{reason}' WHERE id = {session_id}
        ",
            session_id = self.session_id
        ));

        if res.is_err() {
            log::warn!("could not terminate charging session");
        };

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
            env_logger::init();
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
        assert_eq!(ChargingSession::get_last_active().as_ref(), Some(&cs));

        cs.stop(Utc::now(), initial_energy + 25, "SuspendedEV");
        assert_eq!(ChargingSession::get_last_active(), None);
        println!("charging session: {cs:?}");
    }
}
