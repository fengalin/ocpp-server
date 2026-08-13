use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use std::{
    fs,
    sync::{LazyLock, Mutex, MutexGuard},
};

use crate::{Bms, ChargingSession, ChargingSessionSnapshot, ChargingSessionState};

const SQLITE_PATH: &str = "./charging_sessions.sqlite";
static DATABASE: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let existed = fs::exists(SQLITE_PATH).expect("valid path");
    let db = Connection::open(SQLITE_PATH).expect("be able to create the db");

    if !existed {
        db.execute_batch("
            BEGIN;
            CREATE TABLE charging_session (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                state STRING,
                battery_capacity FLOAT,
                soc_cap FLOAT
            );
            CREATE TABLE charging_session_snapshot (
                timestamp STRING,
                sessionid INTEGER,
                energy INTEGER,
                power INTEGER,
                l1_voltage INTEGER,
                temperature INTEGER,
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
        let Some((session_id, battery_capacity, soc_cap)) = self
            .0
            .query_row::<(i32, f64, Option<f64>), _, _>(
                &format!(
                    "SELECT id, battery_capacity, soc_cap FROM charging_session
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='charging_session'
                    )
                    AND state == '{}';",
                    ChargingSessionState::Active
                ),
                [],
                |row| {
                    let session_id = row.get(0)?;
                    let battery_capacity = row.get(1)?;
                    let soc_cap = row.get(2)?;
                    Ok((session_id, battery_capacity, soc_cap))
                },
            )
            .optional()
            .context("querying last active charging session")?
        else {
            log::info!("no active charging sessions");
            return Ok(None);
        };

        let mut stmt = self
            .0
            .prepare(
                "
                SELECT * FROM charging_session_snapshot
                WHERE sessionid == ?1;
            ",
            )
            .unwrap();
        let mut rows = stmt
            .query([session_id])
            .context("getting last active charging session snapshots")?;

        let mut cs = None;
        while let Some(row) = rows
            .next()
            .context("getting last active charging session row")?
        {
            let timestamp = row.get_unwrap::<_, DateTime<Utc>>("timestamp");
            let energy = row.get_unwrap::<_, i64>("energy") as u64;
            let power = row.get_unwrap::<_, Option<i64>>("power").map(|p| p as u64);
            let l1_voltage = row
                .get_unwrap::<_, Option<i64>>("l1_voltage")
                .map(|v| v as u64);
            let temperature = row
                .get_unwrap::<_, Option<i64>>("temperature")
                .map(|p| p as u64);
            let soc = row.get_unwrap::<_, Option<f64>>("soc");

            let cs = cs.get_or_insert_with(|| {
                let session_id = row.get_unwrap::<_, i64>("sessionid") as i32;
                log::info!("## found active session: {session_id}, timestamp: {timestamp}");

                ChargingSession::from_database(
                    session_id,
                    energy,
                    Bms {
                        capacity: battery_capacity,
                        initial_soc: soc,
                        soc_cap,
                    },
                    ChargingSessionState::Active,
                )
            });

            cs.add_snapshot_from_database(
                ChargingSessionSnapshot::builder(timestamp, energy)
                    .power(power)
                    .l1_voltage(l1_voltage)
                    .temperature(temperature)
                    .soc(soc)
                    .build(),
            );
        }

        Ok(cs)
    }

    pub fn add_new_charging_session(&self, bms: Bms) -> i32 {
        self.0
            .query_one::<i32, _, _>(
                "INSERT INTO charging_session (state, battery_capacity, soc_cap)
                    VALUES (:state, :battery_capacity, :soc_cap)
                    RETURNING rowid;",
                named_params![
                    ":state": ChargingSessionState::Active.to_string(),
                    ":battery_capacity": bms.capacity,
                    ":soc_cap": bms.soc_cap,
                ],
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
            (timestamp, sessionid, energy, power, l1_voltage, temperature, soc)
            VALUES (
                :timestamp,
                :session_id,
                :energy,
                :power,
                :l1_voltage,
                :temperature,
                :soc
            );
        ",
            named_params![
                ":timestamp": snapshot.timestamp,
                ":session_id": session_id,
                ":energy": snapshot.energy as i64,
                ":power": snapshot.power.map(|p| p as i64),
                ":l1_voltage": snapshot.l1_voltage.map(|v| v as i64),
                ":temperature": snapshot.temperature.map(|t| t as i64),
                ":soc": snapshot.soc,
            ],
        );

        if let Err(err) = res {
            log::error!("could not insert charging session snapshot: {err}");
        };
    }
}
