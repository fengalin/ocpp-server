use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use std::{
    fs,
    sync::{LazyLock, Mutex, MutexGuard},
};

use crate::{
    Bms, ChargingSchedule, ChargingSchedulePeriod, ChargingScheduleState, ChargingSession,
    ChargingSessionSnapshot, ChargingSessionState,
};

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
                transaction_id INTEGER
            );
            CREATE TABLE charging_session_snapshot (
                timestamp STRING,
                sessionid INTEGER,
                is_reference INTEGER,
                energy INTEGER,
                power INTEGER,
                l1_voltage INTEGER,
                temperature INTEGER,
                soc FLOAT,
                soc_cap FLOAT,
                FOREIGN KEY (sessionid) REFERENCES charging_session(id)
            );
            CREATE INDEX charging_session_snapshot_session_id ON charging_session_snapshot (sessionid);
            CREATE TABLE schedule (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                set_time STRING,
                state i32
            );
            CREATE TABLE schedule_period (
                scheduleid INTEGER,
                start STRING,
                end STRING,
                power_limit FLOAT,
                FOREIGN KEY (scheduleid) REFERENCES schedule(id)
            );
            CREATE INDEX schedule_period_schedule_id ON schedule_period (scheduleid);
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

    pub fn get_last_charging_session(&self, bms: &Bms) -> anyhow::Result<Option<ChargingSession>> {
        let Some((session_id, state, transaction_id)) = self
            .0
            .query_row::<(i32, String, Option<i32>), _, _>(
                "SELECT id, state, transaction_id FROM charging_session
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='charging_session'
                    )",
                [],
                |row| {
                    let session_id = row.get(0)?;
                    let state = row.get(1)?;
                    let transaction_id = row.get(2)?;
                    Ok((session_id, state, transaction_id))
                },
            )
            .optional()
            .context("querying last charging session")?
        else {
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
            .context("getting last charging session snapshots")?;

        let mut cs = None;
        while let Some(row) = rows.next().context("getting last charging session row")? {
            let timestamp = row.get_unwrap::<_, DateTime<Utc>>("timestamp");
            let is_reference = row.get_unwrap::<_, i64>("is_reference") == 1;
            let energy = row.get_unwrap::<_, i64>("energy") as u64;
            let power = row.get_unwrap::<_, Option<i64>>("power").map(|p| p as u64);
            let l1_voltage = row
                .get_unwrap::<_, Option<i64>>("l1_voltage")
                .map(|v| v as u64);
            let temperature = row
                .get_unwrap::<_, Option<i64>>("temperature")
                .map(|p| p as u64);
            let soc = row.get_unwrap::<_, Option<f64>>("soc");
            let soc_cap = row.get_unwrap::<_, Option<f64>>("soc_cap");

            let cs = cs.get_or_insert_with(|| {
                ChargingSession::from_database(
                    bms.clone(),
                    session_id,
                    transaction_id,
                    state.parse().expect("infallible"),
                )
            });

            cs.add_snapshot_from_database(
                ChargingSessionSnapshot::builder(timestamp, energy)
                    .is_reference(is_reference)
                    .power(power)
                    .l1_voltage(l1_voltage)
                    .temperature(temperature)
                    .soc(soc)
                    .soc_cap(soc_cap)
                    .build(),
            );
        }

        if let Some(ref mut cs_mut) = cs {
            cs_mut.done_retrieving_snapshots_from_db(self);
        }

        Ok(cs)
    }

    pub fn add_new_charging_session(
        &self,
        state: ChargingSessionState,
        transaction_id: Option<i32>,
    ) -> i32 {
        self.0
            .query_one::<i32, _, _>(
                // FIXME RETURNING id?
                "INSERT INTO charging_session (state, transaction_id)
                    VALUES (:state, :transaction_id)
                    RETURNING id;",
                named_params![
                    ":state": state.to_string(),
                    ":transaction_id": transaction_id.map(|tid| tid as i64),
                ],
                |row| row.get(0),
            )
            .unwrap()
    }

    pub fn set_charging_session_state(&self, session_id: i32, state: &ChargingSessionState) {
        let res = self.0.execute(
            "UPDATE charging_session SET state = :state WHERE id = :session_id;",
            named_params![
                ":state": state.to_string(),
                ":session_id": session_id,
            ],
        );

        if let Err(err) = res {
            log::error!("could not terminate charging session: {err}");
        };
    }

    pub fn stop_charging_session(&self, session_id: i32, reason: &ChargingSessionState) {
        self.set_charging_session_state(session_id, reason);
    }

    pub fn add_charging_session_snapshot(
        &self,
        session_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) {
        let res = self.0.execute(
            "
            INSERT INTO charging_session_snapshot
            (timestamp, sessionid, is_reference, energy, power, l1_voltage, temperature, soc, soc_cap)
            VALUES (
                :timestamp,
                :session_id,
                :is_reference,
                :energy,
                :power,
                :l1_voltage,
                :temperature,
                :soc,
                :soc_cap
            );
        ",
            named_params![
                ":timestamp": snapshot.timestamp,
                ":session_id": session_id,
                ":is_reference": snapshot.is_reference,
                ":energy": snapshot.energy as i64,
                ":power": snapshot.power.map(|p| p as i64),
                ":l1_voltage": snapshot.l1_voltage.map(|v| v as i64),
                ":temperature": snapshot.temperature.map(|t| t as i64),
                ":soc": snapshot.soc,
                ":soc_cap": snapshot.soc_cap,
            ],
        );

        if let Err(err) = res {
            log::error!("could not insert charging session snapshot: {err}");
        };
    }

    pub fn get_last_charging_schedule(&self) -> anyhow::Result<Option<ChargingSchedule>> {
        let Some((schedule_id, set_time, state)) = self
            .0
            .query_row::<(i32, DateTime<Utc>, i32), _, _>(
                "SELECT id, set_time, state FROM schedule
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='schedule'
                    )",
                [],
                |row| {
                    let schedule_id = row.get(0)?;
                    let set_time = row.get(1)?;
                    let state = row.get(2)?;
                    Ok((schedule_id, set_time, state))
                },
            )
            .optional()
            .context("querying last charging schedule")?
        else {
            return Ok(None);
        };

        let mut stmt = self
            .0
            .prepare(
                "
                SELECT * FROM schedule_period
                WHERE scheduleid == ?1;
            ",
            )
            .unwrap();
        let mut rows = stmt
            .query([schedule_id])
            .context("getting last charging schedule periods")?;

        let mut schedule = None;
        while let Some(row) = rows
            .next()
            .context("getting last charging schedule period row")?
        {
            let start = row.get_unwrap::<_, DateTime<Utc>>("start");
            let end = row.get_unwrap::<_, DateTime<Utc>>("end");
            let power_limit = row.get_unwrap::<_, f64>("power_limit");

            let schedule = schedule.get_or_insert_with(|| ChargingSchedule {
                id: schedule_id,
                set_time,
                state: state.into(),
                ..Default::default()
            });

            schedule.periods.insert(
                start.naive_utc(),
                ChargingSchedulePeriod {
                    start: start.naive_utc(),
                    end: end.naive_utc(),
                    limit: power_limit,
                },
            );
        }

        Ok(schedule)
    }

    pub fn add_new_charging_schedule(&self, schedule: &ChargingSchedule) -> i32 {
        let schedule_id = self
            .0
            .query_one::<i32, _, _>(
                // FIXME RETURNING id?
                "INSERT INTO schedule (set_time, state)
                    VALUES (:set_time, :state)
                    RETURNING id;",
                named_params![
                    ":set_time": schedule.set_time,
                    ":state": schedule.state.as_i32(),
                ],
                |row| row.get(0),
            )
            .unwrap();

        for period in schedule.periods.values() {
            let res = self.0.execute(
                "
                INSERT INTO schedule_period
                (scheduleid, start, end, power_limit)
                VALUES (
                    :schedule_id,
                    :start,
                    :end,
                    :power_limit
                );
            ",
                named_params![
                    ":schedule_id": schedule_id,
                    ":start": period.start.and_utc(),
                    ":end": period.end.and_utc(),
                    ":power_limit": period.limit,
                ],
            );

            if let Err(err) = res {
                log::error!("could not insert charging schedule period: {err}");
            };
        }

        schedule_id
    }

    pub fn inactivate_charging_schedule(&self, schedule_id: i32) {
        let res = self.0.execute(
            "UPDATE schedule SET state = :state WHERE id = :schedule_id;",
            named_params![
                ":state": ChargingScheduleState::Inactive.as_i32(),
                ":schedule_id": schedule_id,
            ],
        );

        if let Err(err) = res {
            log::error!("could not inactivate charging schedule: {err}");
        };
    }
}
