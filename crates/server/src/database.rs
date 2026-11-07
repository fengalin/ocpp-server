use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use std::{
    fs,
    sync::{LazyLock, Mutex, MutexGuard},
};
use tracing::error;

use charge_system_common_model::{
    bms::{Bms, SoC},
    plan_model::{ChargingPeriod, ChargingPlan},
};

use crate::{
    ChargingSchedule, ChargingScheduleState, ChargingSession, ChargingSessionSnapshot,
    ChargingSessionState,
};

static DATABASE: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let database_path = cfg_select! {
        test => std::path::PathBuf::from("charging_sessions.sqlite"),
        _ => {{
            let proj_dirs = directories::ProjectDirs::from("org", "fengalin", env!("CARGO_PKG_NAME"))
                .unwrap();
            let db_dir = proj_dirs.data_dir();
            fs::DirBuilder::new().recursive(true).create(db_dir).expect("write access to data dir");
            db_dir.join("charging_sessions.sqlite")
        }}
    };
    let existed = fs::exists(database_path.as_path()).expect("valid path");
    let db = Connection::open(database_path).expect("be able to create the db");

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
                is_absolute_soc INTERGER,
                soc FLOAT,
                target_soc FLOAT,
                FOREIGN KEY (sessionid) REFERENCES charging_session(id)
            );
            CREATE INDEX charging_session_snapshot_session_id ON charging_session_snapshot (sessionid);
            CREATE TABLE schedule (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                set_time STRING,
                plan_kind STRING,
                deadline STRING,
                target_soc FLOAT,
                power_limit INTEGER,
                state i32
            );
            CREATE TABLE schedule_period (
                scheduleid INTEGER,
                start STRING,
                duration INTEGER,
                power_limit INTEGER,
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
        Database(DATABASE.lock().unwrap_or_else(|err| {
            // A panic while the guard was held poisons the mutex. The
            // connection itself is still consistent (each statement is
            // atomic and rusqlite rolls back an open transaction on drop),
            // so recover loudly instead of panicking on every later access.
            error!("database mutex was poisoned by a panic; recovering the connection");
            err.into_inner()
        }))
    }

    /// Get last charging session, and add update Bms if applicable
    pub fn get_last_charging_session(
        &self,
        bms: &mut Bms,
    ) -> anyhow::Result<Option<ChargingSession>> {
        let Some((session_id, state, transaction_id)) = self
            .0
            .query_row::<(i32, String, i32), _, _>(
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
            let power = row.get_unwrap::<_, Option<i64>>("power").map(|p| p as u32);
            let l1_voltage = row
                .get_unwrap::<_, Option<i64>>("l1_voltage")
                .map(|v| v as u16);
            let temperature = row
                .get_unwrap::<_, Option<i64>>("temperature")
                .map(|p| p as u16);
            let is_absolute_soc = row.get_unwrap::<_, i64>("is_absolute_soc") != 0;
            let soc = row.get_unwrap::<_, Option<f64>>("soc").map(|s| s as f32);
            let target_soc = row
                .get_unwrap::<_, Option<f64>>("target_soc")
                .map(|t| t as f32);

            let cs = cs.get_or_insert_with(|| {
                ChargingSession::from_database(
                    bms.capacity() as u32,
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
                    .soc(SoC::new(is_absolute_soc, soc))
                    .target_soc(target_soc)
                    .build(),
            );
        }

        if let Some(ref cs) = cs {
            if let Some(first) = cs.first_snapshot() {
                let last = cs.last_snapshot().expect("at least one");
                if cs.is_complete() {
                    bms.init_from_db(last.energy, last.energy, SoC::Unknown);
                } else {
                    bms.init_from_db(first.energy, last.energy, last.soc);
                }
            } else {
                error!(session_id = cs.session_id(), "FIXME: no first snapshot");
            }
        }

        Ok(cs)
    }

    fn insert_session(
        conn: &Connection,
        state: &ChargingSessionState,
        transaction_id: i32,
    ) -> anyhow::Result<i32> {
        Ok(conn.query_one::<i32, _, _>(
            "INSERT INTO charging_session (state, transaction_id)
                VALUES (:state, :transaction_id)
                RETURNING id;",
            named_params![
                ":state": state.to_string(),
                ":transaction_id": transaction_id as i64,
            ],
            |row| row.get(0),
        )?)
    }

    /// Insert a new session and its first snapshot atomically: a session
    /// without any snapshot would break recovery (`Evse::new` expects at
    /// least one), so the two writes must commit or roll back together.
    pub fn add_charging_session_with_snapshot(
        &self,
        state: ChargingSessionState,
        transaction_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) -> anyhow::Result<i32> {
        let tx = self.0.unchecked_transaction()?;
        let session_id = Self::insert_session(&tx, &state, transaction_id)?;
        Self::insert_session_snapshot(&tx, session_id, snapshot)?;
        tx.commit()?;
        Ok(session_id)
    }

    fn insert_session_snapshot(
        conn: &Connection,
        session_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) -> anyhow::Result<()> {
        conn.execute(
            "
            INSERT INTO charging_session_snapshot
            (
                timestamp,
                sessionid,
                is_reference,
                energy,
                power,
                l1_voltage,
                temperature,
                is_absolute_soc,
                soc,
                target_soc
            )
            VALUES (
                :timestamp,
                :session_id,
                :is_reference,
                :energy,
                :power,
                :l1_voltage,
                :temperature,
                :is_absolute_soc,
                :soc,
                :target_soc
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
                ":is_absolute_soc": snapshot.soc.is_absolute(),
                ":soc": snapshot.soc.inner(),
                ":target_soc": snapshot.target_soc,
            ],
        )?;
        Ok(())
    }

    pub fn add_charging_session_snapshot(
        &self,
        session_id: i32,
        snapshot: &ChargingSessionSnapshot,
    ) -> anyhow::Result<()> {
        Self::insert_session_snapshot(&self.0, session_id, snapshot)
    }

    fn update_session_state(
        conn: &Connection,
        session_id: i32,
        state: &ChargingSessionState,
    ) -> anyhow::Result<()> {
        conn.execute(
            "UPDATE charging_session SET state = :state WHERE id = :session_id;",
            named_params![
                ":state": state.to_string(),
                ":session_id": session_id,
            ],
        )?;
        Ok(())
    }

    pub fn set_charging_session_state(&self, session_id: i32, state: &ChargingSessionState) {
        if let Err(err) = Self::update_session_state(&self.0, session_id, state) {
            error!("could not set charging session state: {err}");
        }
    }

    pub fn stop_charging_session(&self, session_id: i32, reason: &ChargingSessionState) {
        self.set_charging_session_state(session_id, reason);
    }

    pub fn get_last_charging_schedule(&self) -> anyhow::Result<ChargingSchedule> {
        let Some((schedule_id, set_time, plan_kind, deadline, target_soc, power_limit, state)) = self
            .0
            .query_row::<(i32, DateTime<Utc>, String, Option<NaiveDateTime>, Option<f32>, Option<i64>, i32), _, _>(
                "SELECT id, set_time, plan_kind, deadline, target_soc, power_limit, state FROM schedule
                    WHERE id IN (
                        SELECT seq FROM sqlite_sequence WHERE name='schedule'
                    )",
                [],
                |row| {
                    let schedule_id = row.get(0)?;
                    let set_time = row.get(1)?;
                    let plan_kind = row.get(2)?;
                    let deadline = row.get(3)?;
                    let target_soc = row.get(4)?;
                    let power_limit = row.get(5)?;
                    let state = row.get(6)?;
                    Ok((schedule_id, set_time, plan_kind, deadline, target_soc, power_limit, state))
                },
            )
            .optional()
            .context("querying last charging schedule")?
        else {
            return Ok(ChargingSchedule::new_unlimited_active());
        };

        let mut schedule = ChargingSchedule {
            id: schedule_id,
            set_time,
            plan: ChargingPlan::from_db(&plan_kind, deadline, target_soc, power_limit)?,
            state: ChargingScheduleState::from(state),
            ..Default::default()
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

        while let Some(row) = rows
            .next()
            .context("getting last charging schedule period row")?
        {
            let start = row.get_unwrap::<_, DateTime<Utc>>("start").into();
            let duration_s = row.get_unwrap::<_, i64>("duration");
            let power_limit = row.get_unwrap::<_, i64>("power_limit");

            schedule.periods.insert(
                start,
                ChargingPeriod {
                    start,
                    duration_s: duration_s as _,
                    power_limit: power_limit.try_into().context("power limit")?,
                },
            );
        }

        Ok(schedule)
    }

    fn insert_schedule_priv(
        conn: &Connection,
        schedule: &mut ChargingSchedule,
    ) -> anyhow::Result<()> {
        let schedule_id = conn.query_one::<i32, _, _>(
            "INSERT INTO schedule (set_time, plan_kind, deadline, target_soc, power_limit, state)
                VALUES (:set_time, :plan_kind, :deadline, :target_soc, :power_limit, :state)
                RETURNING id;",
            named_params![
                ":set_time": schedule.set_time,
                ":plan_kind": schedule.plan.kind().to_string(),
                ":deadline": schedule.plan.deadline().map(|d| d.to_utc().naive_local()),
                ":target_soc": schedule.plan.target_soc(),
                ":power_limit": schedule.plan.power_limit(),
                ":state": schedule.state.as_i32(),
            ],
            |row| row.get(0),
        )?;

        // A schedule without its periods would be inconsistent (the periods
        // are what `remaining()` computes from), so a period insert failure
        // must fail the whole insert — hence `?` here, not a logged skip.
        for period in schedule.periods.values() {
            conn.execute(
                "
                INSERT INTO schedule_period
                (scheduleid, start, duration, power_limit)
                VALUES (
                    :schedule_id,
                    :start,
                    :duration,
                    :power_limit
                );
            ",
                named_params![
                    ":schedule_id": schedule_id,
                    ":start": period.start.to_utc(),
                    ":duration": period.duration_s as i64,
                    ":power_limit": period.power_limit,
                ],
            )?;
        }

        schedule.id = schedule_id;

        Ok(())
    }

    fn inactivate_last_schedule(conn: &Connection) -> anyhow::Result<()> {
        conn.execute(
            "UPDATE schedule SET state = :state
                WHERE id IN (
                    SELECT seq FROM sqlite_sequence WHERE name='schedule'
                )",
            named_params![
                ":state": ChargingScheduleState::Inactive.as_i32(),
            ],
        )?;
        Ok(())
    }

    /// Inactivate the previous schedule and insert the new one (with its
    /// periods) atomically, so a failure can't leave both or neither active.
    pub fn insert_charging_schedule(&self, schedule: &mut ChargingSchedule) -> anyhow::Result<()> {
        let tx = self.0.unchecked_transaction()?;
        Self::inactivate_last_schedule(&tx)?;
        Self::insert_schedule_priv(&tx, schedule)?;
        tx.commit()?;
        Ok(())
    }
}
