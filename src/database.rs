use anyhow::Context;
use chrono::{DateTime, Local};
use rusqlite::{Connection, named_params};
use std::{
    fs,
    sync::{LazyLock, Mutex, MutexGuard},
};

use crate::{ChargingSession, ChargingSessionSnapshot, ChargingSessionState};

const SQLITE_PATH: &str = "./charging_sessions.sqlite";
static DATABASE: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let existed = fs::exists(SQLITE_PATH).expect("valid path");
    let db = Connection::open(SQLITE_PATH).expect("be able to create the db");

    if !existed {
        db.execute_batch("
            BEGIN;
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
            CREATE INDEX charging_session_snapshot_session_id ON charging_session_snapshot (sessionid);
            COMMIT;
        ").unwrap();
    }

    Mutex::new(db)
});

pub struct Database<'a>(MutexGuard<'a, Connection>);

impl<'a> Database<'a> {
    pub fn get() -> Self {
        Database(DATABASE.lock().unwrap())
    }

    pub fn get_last_active_charging_session(&self) -> anyhow::Result<Option<ChargingSession>> {
        let mut stmt = self
            .0
            .prepare(&format!(
                "
                SELECT * FROM charging_session_snapshot
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
        let mut rows = stmt
            .query([])
            .context("getting last active charging session")?;

        let mut cs = None;
        while let Some(row) = rows
            .next()
            .context("getting last active charging session row")?
        {
            let timestamp = row.get_unwrap::<_, DateTime<Local>>("timestamp");
            let energy = row.get_unwrap::<_, i64>("energy") as u64;
            let power = row.get_unwrap::<_, i64>("power") as u64;
            let soc = row.get_unwrap::<_, Option<f64>>("soc");

            let cs = cs.get_or_insert_with(|| {
                let session_id = row.get_unwrap::<_, i64>("sessionid") as i32;
                log::info!("## found active session: {session_id}, timestamp: {timestamp}");

                ChargingSession::from_database(
                    session_id,
                    energy,
                    soc,
                    ChargingSessionState::Active,
                )
            });

            cs.add_snapshot_from_database(timestamp, energy, power, soc);
        }

        Ok(cs)
    }

    pub fn add_new_charging_session(&self) -> i32 {
        self.0
            .query_one::<i32, _, _>(
                &format!(
                    "INSERT INTO charging_session (state) VALUES ('{}') RETURNING rowid;",
                    ChargingSessionState::Active
                ),
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    pub fn stop_charging_session(&self, session_id: i32, reason: &ChargingSessionState) {
        let res = self.0.execute(
            "UPDATE charging_session SET state = :state WHERE id = :session_id;",
            named_params![
                ":state": reason.to_string(),
                ":session_id": session_id,
            ],
        );

        if let Err(err) = res {
            log::error!("could not terminate charging session: {err}");
        };
    }

    pub fn add_charging_session_snapshot(
        &self,
        session_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) {
        let res = self.0.execute(
            "
            INSERT INTO charging_session_snapshot
            (timestamp, sessionid, energy, power, soc)
            VALUES (
                :timestamp,
                :session_id,
                :energy,
                :power,
                :soc
            );
        ",
            named_params![
                ":timestamp": snapshot.timestamp,
                ":session_id": session_id,
                ":energy": snapshot.energy as i64,
                ":power": snapshot.power as i64,
                ":soc": snapshot.soc,
            ],
        );

        if let Err(err) = res {
            log::error!("could not insert charging session snapshot: {err}");
        };
    }
}
