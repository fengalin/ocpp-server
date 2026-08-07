use chrono::{DateTime, Local, Utc};
use std::{
    fs,
    sync::{LazyLock, Mutex, MutexGuard},
};

use crate::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};

const SQLITE_PATH: &str = "../charging_sessions.sqlite";
static DATABASE: LazyLock<Mutex<sqlite::Connection>> = LazyLock::new(|| {
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

pub struct Database<'a>(MutexGuard<'a, sqlite::Connection>);

impl<'a> Database<'a> {
    pub fn get() -> Self {
        Database(DATABASE.lock().unwrap())
    }

    pub fn get_last_active_charging_session(&self) -> Option<ChargingSession> {
        let mut statement = self
            .0
            .prepare(format!(
                "SELECT * FROM charging_session_snapshot
                WHERE sessionid IN (
                    SELECT id FROM charging_session
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='charging_session'
                    )
                    AND state == '{}'
                )
            ",
                ChargingSessionState::Active,
            ))
            .unwrap();

        let mut cs = None;
        while let Ok(sqlite::State::Row) = statement.next() {
            let timestamp = statement.read::<String, _>("timestamp").unwrap();
            let energy = statement.read::<i64, _>("energy").unwrap() as u64;
            let power = statement.read::<i64, _>("power").unwrap() as u64;
            let soc = statement.read::<f64, _>("soc").unwrap();
            let soc = (soc > 0.0).then_some(soc);

            let cs = cs.get_or_insert_with(|| {
                let session_id = statement.read::<i64, _>("sessionid").unwrap() as i32;
                log::info!(
                    "## found active session: {session_id}, timestamp: {}",
                    timestamp
                        .parse::<DateTime::<Utc>>()
                        .unwrap()
                        .with_timezone(&Local),
                );

                ChargingSession::from_database(
                    session_id,
                    energy,
                    soc,
                    ChargingSessionState::Active,
                )
            });

            cs.add_snapshot_from_database(&timestamp, energy, power, soc);
        }

        cs
    }

    pub fn add_new_charging_session(&self) -> i32 {
        let mut statement = self
            .0
            .prepare(format!(
                "INSERT INTO charging_session (state) VALUES ('{}') RETURNING rowid;",
                ChargingSessionState::Active
            ))
            .unwrap();
        match statement.next() {
            Ok(sqlite::State::Row) => (),
            other => {
                panic!("failed to insert charging session: {other:?}");
            }
        };

        statement.read::<i64, _>("id").expect("charging session id") as _
    }

    pub fn stop_charging_session(&self, session_id: i32, reason: &ChargingSessionState) {
        let res = self.0.execute(format!(
            "UPDATE charging_session SET state = '{reason}' WHERE id = {session_id}"
        ));

        if res.is_err() {
            log::warn!("could not terminate charging session");
        };
    }

    pub fn add_charging_session_snapshot(
        &self,
        session_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) {
        let res = self.0.execute(format!(
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
            timestamp = snapshot.timestamp,
            energy = snapshot.energy,
            power = snapshot.power,
            soc = snapshot.soc.unwrap_or(-1.0),
        ));

        if let Err(err) = res {
            log::warn!("could not insert charging session snapshot: {err}");
        };
    }
}
