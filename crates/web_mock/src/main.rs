//! Verification harness for the WASM front end.
//!
//! Re-implements the front-end-facing HTTP surface (JSON REST + SSE)
//! using the SAME shared types (`charge-system-web-model` /
//! `charge-system-common-model`) and JSON wire format as the server's webserver,
//! and serves `../static`. It feeds a synthetic charging session so the UI has
//! live data to render. NOT part of the shipped server.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Duration, Local, NaiveTime, Timelike, Utc};
use futures::stream;
use serde::Deserialize;
use tokio::sync::{Mutex, broadcast, mpsc};
use tower_http::ServiceExt;
use tower_http::services::ServeDir;

use charge_system_common_model::{
    bms::{self, Bms},
    meter_value::MeterValueSelection,
    model::{EventLogEntry, Ts},
    notification::{ChargeProgress, ChargeStatus, Notification},
    plan,
    plan_model::{AppliedChargingPlan, ChargingPlan, PlanKind},
};
use charge_system_web_model::model::{MAX_EVENTS, MAX_METER_VALUES, ServerState};

// Relative to the CWD when run via `cd crates/web_mock && cargo run`.
const STATIC_DIR: &str = "../../static";
const BROADCAST_CAPACITY: usize = 512;

/// SoC is a fraction 0–1 internally; the notification boundary (evse.rs)
/// rounds to 3 decimals before it reaches the wire. The mock mirrors that.
fn round_soc(soc: f32) -> f32 {
    (soc * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------
// World (mirrors src/webserver/world::World)
// ---------------------------------------------------------------------

struct World {
    bms: Bms,
    last_ts: DateTime<Utc>,
    charging_plan: Option<AppliedChargingPlan>,
    initial_soc: Option<f32>,
    energy: u64,
    meter_values: VecDeque<MeterValueSelection>,
    events: VecDeque<EventLogEntry>,
}

impl World {
    /// The current charge status: the newest event in the log (mirrors the
    /// server's `World::status`). `Available` until the first one arrives.
    fn status(&self) -> ChargeStatus {
        self.events
            .back()
            .map(|e| e.status.clone())
            .unwrap_or(ChargeStatus::Available)
    }

    fn server_state(&self) -> ServerState {
        ServerState {
            bms: self.bms.clone(),
            ts: Ts::from_utc(self.last_ts),
            status: self.status(),
            charging_plan: self.charging_plan.clone(),
        }
    }

    fn push_sample(&mut self, mv: MeterValueSelection) {
        let ts = mv.ts.to_utc();
        let energy = mv.energy;
        self.meter_values.push_back(mv);
        self.meter_values.retain_back(MAX_METER_VALUES);
        self.last_ts = ts;
        if let Some(e) = energy {
            self.energy = e;
        }
    }

    /// Mirrors the server's retention rule (`World::push_event` in
    /// `webserver/world.rs`): a charging phase keeps its start entry plus
    /// one live entry; every other status appends. Capped at `MAX_EVENTS`.
    fn push_event(&mut self, e: EventLogEntry) {
        let mut trailing_charging = 0;
        for ev in self.events.iter().rev() {
            if matches!(ev.status, ChargeStatus::Charging(_)) {
                trailing_charging += 1;
            } else {
                break;
            }
        }
        if matches!(&e.status, ChargeStatus::Charging(_)) && trailing_charging >= 2 {
            *self.events.back_mut().expect("live Charging entry") = e;
            return;
        }
        self.events.push_back(e);
        self.events.retain_back(MAX_EVENTS);
    }
}

// ---------------------------------------------------------------------
// State
// ---------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    world: Arc<Mutex<World>>,
    front_end_tx: broadcast::Sender<Notification>,
    cmd_tx: mpsc::Sender<String>,
}

// ---------------------------------------------------------------------
// JSON helpers (mirror src/webserver/api.rs)
// ---------------------------------------------------------------------

fn json<T: serde::Serialize>(value: &T) -> Response {
    let bytes = serde_json::to_vec(value).expect("wire type serializes infallibly");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

struct JsonBody<T>(T);

impl<S, T> axum::extract::FromRequest<S> for JsonBody<T>
where
    T: for<'de> serde::Deserialize<'de>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(
        req: axum::http::Request<Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Same bounded read as the real server's `JsonBody`.
        let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
            .await
            .map_err(|e| bad_request(format!("reading body: {e}")))?;
        let value = serde_json::from_slice::<T>(&bytes)
            .map_err(|e| bad_request(format!("json decode: {e}")))?;
        Ok(JsonBody(value))
    }
}

fn bad_request(msg: impl ToString) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn ok_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct MeterValuesQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct EventsQuery {
    #[allow(dead_code)]
    from: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct SetMeterValueSampleIntervalRequest {
    #[allow(dead_code)]
    interval: u16,
}

/// Body of `POST /api/bms/init_soc` (mirrors the real server endpoint, which
/// is a follow-up: here it just seeds the world's init-SoC so the front end's
/// widget can be exercised end-to-end).
#[derive(Deserialize)]
struct SetInitSoc {
    /// Last known state of charge at session start (fraction 0–1).
    soc: f32,
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn router(st: AppState) -> Router {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/metrics", get(get_samples))
        .route("/api/events", get(get_events))
        .route("/api/plans/apply", post(apply_plan))
        .route("/api/plans/stop", post(stop_plan))
        .route("/api/session/stop", post(stop_session))
        .route(
            "/api/cp/set_meter_value_interval",
            post(set_meter_value_interval),
        )
        .route("/api/cp/reboot", post(cp_reboot))
        .route("/api/bms/init_soc", post(set_init_soc))
        .route("/api/stream", get(stream))
        // Revalidate every load, like the real server (see webserver/api.rs).
        .fallback_service(ServeDir::new(STATIC_DIR).override_response_header(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        ))
        .with_state(st)
}

async fn get_state(State(st): State<AppState>) -> Response {
    json(&st.world.lock().await.server_state())
}

async fn get_samples(State(st): State<AppState>, Query(q): Query<MeterValuesQuery>) -> Response {
    let w = st.world.lock().await;
    let from = q.from.unwrap_or(w.last_ts - Duration::hours(1));
    let to = q.to.unwrap_or(w.last_ts);
    let from = from.max(to - Duration::hours(24));
    let (from, to) = (Ts::from_utc(from), Ts::from_utc(to));
    let samples: Vec<MeterValueSelection> = w
        .meter_values
        .iter()
        .filter(|mv| mv.ts >= from && mv.ts <= to)
        .cloned()
        .collect();
    json(&samples)
}

async fn get_events(State(st): State<AppState>, Query(_q): Query<EventsQuery>) -> Response {
    let w = st.world.lock().await;
    let events: Vec<EventLogEntry> = w.events.iter().cloned().collect();
    json(&events)
}

/// Mirror of the server's `evse_plan`: web plan request -> EVSE plan model.
fn evse_plan(req: &charge_system_web_model::model::PlanRequest) -> ChargingPlan {
    match req.kind {
        PlanKind::Deadline => {
            let deadline = req
                .deadline
                .expect("validated")
                .to_utc()
                .with_timezone(&Local);
            ChargingPlan::Deadline {
                deadline: NaiveTime::from_hms_opt(deadline.hour(), deadline.minute(), 0)
                    .expect("valid time of day"),
                target_soc: req.target_soc.expect("validated"),
                power_limit: req.power_limit.expect("validated"),
            }
        }
        PlanKind::Asap => ChargingPlan::Asap {
            target_soc: req.target_soc.expect("validated"),
            power_limit: req.power_limit.expect("validated"),
        },
        PlanKind::Blocking => ChargingPlan::Blocking,
        PlanKind::Unlimited => ChargingPlan::Unlimited,
    }
}

/// Mirrors the server: validate, apply on the "EVSE" (world + BMS target),
/// and let the `ChargingPlan` notification carry the outcome to clients.
#[allow(clippy::result_large_err)] // dev-only harness: errors are full axum Responses
async fn apply_plan(
    State(st): State<AppState>,
    JsonBody(req): JsonBody<charge_system_web_model::model::PlanRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    // Mirror of the server's validation: only Deadline / Asap carry options.
    if matches!(req.kind, PlanKind::Deadline | PlanKind::Asap) {
        let target_soc = *req
            .target_soc
            .as_ref()
            .ok_or_else(|| bad_request("target_soc is required for this plan kind"))?;
        // `!(0 < t && t <= 1)` also rejects NaN.
        if !(0.0 < target_soc && target_soc <= 1.0) {
            return Err(bad_request("target_soc must be in (0, 1]"));
        }
        let power_limit = req.power_limit.unwrap_or(0);
        if power_limit <= bms::STATIC_LOSS {
            return Err(bad_request(format!(
                "power limit must exceed the {} W static loss",
                bms::STATIC_LOSS
            )));
        }
    }
    if req.kind == PlanKind::Deadline && req.deadline.is_none() {
        return Err(bad_request("deadline is required for deadline plans"));
    }
    let plan = evse_plan(&req);
    let applied = {
        let mut w = st.world.lock().await;
        // The server derives its charging schedule from the same shared plan
        // math as the preview; mirror that here (Deadline / Asap only —
        // Blocking / unlimited have no periods).
        let periods = match req.kind {
            PlanKind::Deadline | PlanKind::Asap => {
                let cur_soc = w.bms.current_soc.inner().unwrap_or_default();
                plan::compute(
                    req.kind,
                    Utc::now(),
                    cur_soc,
                    req.target_soc.expect("validated"),
                    req.deadline.map(|t| t.to_utc()),
                    req.power_limit.expect("validated"),
                    &w.bms,
                )
                .periods
            }
            _ => vec![],
        };
        let applied = AppliedChargingPlan { plan, periods };
        // `Unlimited` means "no plan in effect", like the server's world; the
        // BMS target mirrors the active plan (like the server's EVSE).
        w.charging_plan = (!matches!(plan, ChargingPlan::Unlimited)).then(|| applied.clone());
        w.bms.target_soc = applied.plan.target_soc();
        applied
    };
    let _ = st
        .front_end_tx
        .send(Notification::ChargingPlan(applied))
        .ok();
    Ok(ok_json())
}

async fn stop_plan(State(st): State<AppState>) -> Json<serde_json::Value> {
    // Mirror of the server: stop = replace the plan in effect with the
    // default (Blocking) plan.
    let applied = AppliedChargingPlan {
        plan: ChargingPlan::Blocking,
        periods: vec![],
    };
    let mut w = st.world.lock().await;
    w.charging_plan = Some(applied.clone());
    w.bms.target_soc = None;
    drop(w);
    let _ = st.front_end_tx.send(Notification::ChargingPlan(applied));
    ok_json()
}

async fn stop_session(State(st): State<AppState>) -> Json<serde_json::Value> {
    let mut w = st.world.lock().await;
    let ts = w.last_ts;
    w.push_event(EventLogEntry {
        ts: Ts::from_utc(ts),
        status: ChargeStatus::Available,
    });
    ok_json()
}

async fn set_meter_value_interval(
    State(st): State<AppState>,
    JsonBody(_r): JsonBody<SetMeterValueSampleIntervalRequest>,
) -> Json<serde_json::Value> {
    let _ = st.cmd_tx.try_send("set_interval".into());
    ok_json()
}

async fn cp_reboot(State(st): State<AppState>) -> Json<serde_json::Value> {
    let _ = st.cmd_tx.try_send("reboot".into());
    ok_json()
}

#[allow(clippy::result_large_err)] // dev-only harness: errors are full axum Responses
async fn set_init_soc(
    State(st): State<AppState>,
    JsonBody(req): JsonBody<SetInitSoc>,
) -> Result<Json<serde_json::Value>, Response> {
    if !(0.0..=1.0).contains(&req.soc) {
        return Err(bad_request("soc must be in [0, 1]"));
    }
    let mut w = st.world.lock().await;
    w.initial_soc = Some(req.soc);
    // Re-estimate the current SoC from the corrected init SoC + the session
    // energy delivered so far — the same shared rule the front end applies
    // client-side, so the state catch-up confirms the instant preview. When
    // there is no session and the current SoC is unknown, the initial SoC is
    // used as the current SoC (the server does the same).
    let energy = w
        .energy
        .saturating_sub(w.bms.initial_energy.unwrap_or_default());
    let cur_soc = round_soc(plan::estimate_soc(req.soc, energy, &w.bms));
    w.bms.current_soc = bms::SoC::Absolute(cur_soc);
    Ok(ok_json())
}

async fn stream(State(st): State<AppState>) -> impl IntoResponse {
    let rx = st.front_end_tx.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => {
                let event = Event::default().event(ev.name()).data(ev.to_sse_data());
                Some((Ok::<Event, Infallible>(event), rx))
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("SSE subscriber lagged by {n}, sending resync");
                Some((Ok(Event::default().event("resync").data("resync")), rx))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------
// Synthetic session driver
// ---------------------------------------------------------------------

/// Seeds ~24 h of plausible history and then streams a live charging session
/// (Available -> Preparing -> Charging with samples -> Finished), repeating.
async fn drive(st: AppState) {
    let now = Utc::now();

    // Seed 24 h of history at 10 s cadence (24*3600/10 = 8640 samples).
    // A fraction of samples carries `None` fields (the CP mixes voltage-only
    // and power-only MeterValues), exercising the Option fields end to end.
    {
        let mut w = st.world.lock().await;
        let mut soc = 0.10_f32; // fraction 0-1 internally; rounded to 3 dec at the wire boundary
        let mut energy = 5_000u64; // Wh
        let start = now - Duration::hours(24);
        let mut t = start;
        let mut i = 0u64;
        while t < now {
            let charging = t > now - Duration::hours(3);
            let power = if charging { 7000u32 } else { 0 };
            if charging {
                energy += 20;
                soc = (soc + 0.0001).min(0.90);
            }
            let mv = MeterValueSelection {
                ts: Ts::from_utc(t),
                energy: (i % 7 != 3).then_some(energy),
                power: (i % 5 != 1).then_some(power).filter(|&p| p > 0),
                voltage: (!i.is_multiple_of(3)).then_some(230),
                temperature: (i % 9 != 4).then_some(25),
            };
            w.push_sample(mv);
            t += Duration::seconds(10);
            i += 1;
        }
        w.bms.current_soc = bms::SoC::Absolute(round_soc(soc));
        w.initial_soc = Some(0.10);
        w.push_event(EventLogEntry {
            ts: Ts::from_utc(now - Duration::hours(24)),
            status: ChargeStatus::Available,
        });
    }

    let tx = st.front_end_tx.clone();
    let world = st.world.clone();

    loop {
        // Available — like the server on Finishing -> Available, the default
        // (Blocking) plan is applied between sessions.
        let applied = AppliedChargingPlan {
            plan: ChargingPlan::Blocking,
            periods: vec![],
        };
        {
            let mut w = world.lock().await;
            w.charging_plan = Some(applied.clone());
            w.push_event(EventLogEntry {
                ts: Ts::from_utc(Utc::now()),
                status: ChargeStatus::Available,
            });
        }
        let _ = tx.send(Notification::Status(ChargeStatus::Available));
        let _ = tx.send(Notification::ChargingPlan(applied));
        sleep(StdDuration::from_secs(2)).await;

        // Preparing
        {
            let mut w = world.lock().await;
            w.bms = Bms::builder(bms::DEFAULT_CAPACITY)
                .initial_energy(5_000)
                .initial_soc(bms::SoC::Absolute(0.10))
                .current_soc(bms::SoC::Absolute(0.10))
                .current_energy(5_000)
                .build();
            // An ASAP plan for the session (its target matches the synthetic
            // ramp) is applied before charging starts, like a user would do.
            w.bms.target_soc = Some(0.90);
            let plan = ChargingPlan::Asap {
                target_soc: 0.90,
                power_limit: 7_000,
            };
            // Periods from the same shared plan math as the preview.
            let periods =
                plan::compute(PlanKind::Asap, Utc::now(), 0.10, 0.90, None, 7_000, &w.bms).periods;
            w.charging_plan = Some(AppliedChargingPlan { plan, periods });
            w.push_event(EventLogEntry {
                ts: Ts::from_utc(Utc::now()),
                status: ChargeStatus::Preparing,
            });
            // Like the server: a new session changes the BMS (initial/current
            // energy) — broadcast it so session-energy bookkeeping is correct.
            let _ = tx.send(Notification::BmsUpdate(w.bms.clone()));
            let _ = tx.send(Notification::Status(ChargeStatus::Preparing));
            let _ = tx.send(Notification::ChargingPlan(
                w.charging_plan.clone().expect("set above"),
            ));
        }
        sleep(StdDuration::from_secs(2)).await;

        // Charging, ramp SoC from ~10% toward ~90%
        {
            let mut soc = 0.10_f32; // fraction 0-1 internally; rounded to 3 dec at the wire boundary
            let mut energy = 5_000u64;
            let mut i = 0u64;
            loop {
                let now = Utc::now();
                let mv = MeterValueSelection {
                    ts: Ts::from_utc(now),
                    energy: Some(energy),
                    // Mirror the CP's mixed MeterValues: every 4th sample has
                    // no power measure, every 7th has no temperature.
                    power: (i % 4 != 2).then_some(7000),
                    voltage: Some(230),
                    temperature: (i % 7 != 3).then_some(26),
                };
                {
                    let mut w = world.lock().await;
                    let soc = round_soc(soc);
                    w.bms.current_soc = bms::SoC::Absolute(soc);
                    w.bms.current_energy = Some(energy);
                    w.push_sample(mv.clone());
                }
                let _ = tx.send(Notification::MeterValues(mv));
                // Like the real server, charge progress rides on status
                // updates (here: every 5th sample, 1 s apart), carrying the
                // live SoC and the synthetic ramp's remaining time.
                if i.is_multiple_of(5) {
                    let remaining = (((0.90 - soc) / 0.005).ceil() as u32).max(1);
                    let status = ChargeStatus::Charging(ChargeProgress {
                        soc: round_soc(soc),
                        remaining_time: remaining,
                    });
                    let mut w = world.lock().await;
                    w.push_event(EventLogEntry {
                        ts: Ts::from_utc(Utc::now()),
                        status: status.clone(),
                    });
                    let _ = tx.send(Notification::Status(status));
                }
                energy += 12; // ~12 Wh per sample (7 kW * 1 s / 3600 ~ 2 Wh, scaled up for visibility)
                soc = (soc + 0.005).min(0.90);
                i += 1;
                if soc >= 0.90 {
                    break;
                }
                sleep(StdDuration::from_secs(1)).await;
            }
        }

        // Finished
        {
            let mut w = world.lock().await;
            w.bms.current_soc = bms::SoC::Absolute(0.90);
            w.push_event(EventLogEntry {
                ts: Ts::from_utc(Utc::now()),
                status: ChargeStatus::SuspendedEv,
            });
            let _ = tx.send(Notification::Status(ChargeStatus::SuspendedEv));
        }
        sleep(StdDuration::from_secs(6)).await;
    }
}

async fn sleep(d: StdDuration) {
    tokio::time::sleep(d).await;
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(9180);

    let (front_end_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let world = World {
        bms: Bms::builder(bms::DEFAULT_CAPACITY).build(),
        last_ts: Utc::now(),
        charging_plan: None,
        initial_soc: None,
        energy: 0,
        meter_values: VecDeque::new(),
        events: VecDeque::new(),
    };
    let st = AppState {
        world: Arc::new(Mutex::new(world)),
        front_end_tx,
        cmd_tx,
    };

    let drive_st = st.clone();
    tokio::spawn(async move { drive(drive_st).await });

    let app = router(st);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    println!("charge-system-web-mock listening on http://{addr}/ (static: {STATIC_DIR})");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
