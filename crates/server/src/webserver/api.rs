//! HTTP API: REST endpoints, SSE stream and static file serving.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveTime, Timelike, Utc};
use futures::stream;
use serde::Deserialize;
use tower_http::ServiceExt;
use tower_http::services::ServeDir;
use tracing::info;

use charge_system_common_model::{
    bms,
    meter_value::MeterValueSelection,
    model::{EventLogEntry, Ts},
    plan_model::{ChargingPlan, PlanKind},
};
use charge_system_web_model::model::PlanRequest;

use super::state::AppState;
use crate::ocpp::CommandToChargingPoint;

pub struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    pub fn bad_request(msg: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.to_string(),
        }
    }

    pub fn internal(msg: impl ToString) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: msg.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.msg });
        (self.status, Json(body)).into_response()
    }
}

/// Build a JSON REST response. The whole front-end data plane (REST bodies
/// + SSE payloads) is JSON.
fn json<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .expect("static status + header build infallibly"),
        Err(e) => ApiError::internal(format!("serialize response: {e}")).into_response(),
    }
}

/// Upper bound for a JSON request body (the default `DefaultBodyLimit` of
/// axum's `Json` extractor, which this custom extractor reimplements for a
/// JSON `ApiError` rejection, would be 2 MiB — these bodies are tiny).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Parse a JSON request body into `T`.
struct JsonBody<T>(T);

impl<S, T> axum::extract::FromRequest<S> for JsonBody<T>
where
    T: for<'de> serde::Deserialize<'de>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        req: axum::http::Request<axum::body::Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|e| ApiError::bad_request(format!("reading body: {e}")))?;
        let value = serde_json::from_slice::<T>(&bytes)
            .map_err(|e| ApiError::bad_request(format!("json decode: {e}")))?;
        Ok(JsonBody(value))
    }
}

#[derive(Deserialize)]
struct MeterValuesQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct EventsQuery {
    from: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMeterValueSampleIntervalRequest {
    interval: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetInitSocRequest {
    /// Last known state of charge at the start of the session, as a
    /// *fraction* 0–1 (see `Bms` / `SoC` invariants).
    soc: f32,
}

/// Validate the web plan request and translate it into the EVSE's
/// `ChargingPlan` (the CLI plan model) in one step, so downstream code needs
/// no `.expect("validated above")`.
///
/// The deadline's local time of day is what
/// `ChargingSchedule::try_from_charging_plan` expects (it recomputes the start
/// time from the BMS and picks today or
/// tomorrow), the ASAP kind starts the period now, and Blocking / Unlimited
/// map onto the same-named variants.
fn evse_plan(req: &PlanRequest, now: DateTime<Utc>) -> Result<ChargingPlan, ApiError> {
    // Only the Deadline / Asap kinds carry options; Blocking and Unlimited
    // have none (their `target_soc` / `power_limit` are ignored).
    if matches!(req.kind, PlanKind::Deadline | PlanKind::Asap) {
        // `target_soc` is a *fraction* 0–1 (see `PlanRequest` /
        // `charge-system-web-model`). `!(0 < t && t <= 1)` also rejects NaN.
        let target_soc = *req
            .target_soc
            .as_ref()
            .ok_or_else(|| ApiError::bad_request("target_soc is required for this plan kind"))?;
        if !(0.0 < target_soc && target_soc <= 1.0) {
            return Err(ApiError::bad_request("target_soc must be in (0, 1]"));
        }
        let power_limit = *req
            .power_limit
            .as_ref()
            .ok_or_else(|| ApiError::bad_request("power limit is required for this plan kind"))?;
        // A limit at or below the static loss delivers no power to the
        // battery (`plan::compute` divides by `limit - STATIC_LOSS`).
        if power_limit <= bms::STATIC_LOSS {
            return Err(ApiError::bad_request(format!(
                "power limit must exceed the {} W static loss",
                bms::STATIC_LOSS
            )));
        }
        if req.kind == PlanKind::Deadline {
            let d = req
                .deadline
                .ok_or_else(|| ApiError::bad_request("deadline is required for deadline plans"))?;
            if d <= Ts::from_utc(now) {
                return Err(ApiError::bad_request("deadline must be in the future"));
            }
            if d > Ts::from_utc(now) + chrono::Duration::hours(48) {
                return Err(ApiError::bad_request(
                    "deadline must be within the next 48 h",
                ));
            }
            let deadline = d.to_utc().with_timezone(&Local);
            Ok(ChargingPlan::Deadline {
                deadline: NaiveTime::from_hms_opt(deadline.hour(), deadline.minute(), 0)
                    .expect("hour/minute of a valid DateTime are a valid time of day"),
                target_soc,
                power_limit,
            })
        } else {
            Ok(ChargingPlan::Asap {
                target_soc,
                power_limit,
            })
        }
    } else {
        Ok(match req.kind {
            PlanKind::Blocking => ChargingPlan::Blocking,
            PlanKind::Unlimited => ChargingPlan::Unlimited,
            _ => unreachable!("checked above"),
        })
    }
}

pub fn router(st: AppState) -> Router {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/metrics", get(get_samples))
        .route("/api/events", get(get_events))
        .route("/api/plans/apply", post(apply_plan))
        .route("/api/plans/stop", post(stop_plan))
        .route("/api/session/stop", post(stop_session))
        .route("/api/bms/init_soc", post(set_init_soc))
        .route(
            "/api/cp/set_meter_value_interval",
            post(set_meter_value_interval),
        )
        .route("/api/cp/reboot", post(cp_reboot))
        .route("/api/stream", get(stream))
        // Revalidate every load (ETag -> 304 when unchanged): without this,
        // phone browsers heuristically cache styles.css and serve a stale one
        // across dev iterations.
        .fallback_service(ServeDir::new("static").override_response_header(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        ))
        .with_state(st)
}

// ---------------------------------------------------------------------
// state + history
// ---------------------------------------------------------------------

async fn get_state(State(st): State<AppState>) -> Response {
    // Extract under the lock, serialize after releasing it.
    let state = st.world.lock().await.server_state();
    json(&state)
}

async fn get_samples(State(st): State<AppState>, Query(q): Query<MeterValuesQuery>) -> Response {
    let samples = {
        let w = st.world.lock().await;
        let from = q.from.unwrap_or(w.last_ts - chrono::Duration::hours(1));
        let to = q.to.unwrap_or(w.last_ts);
        // Clamp the window to 24 h so a client can't request the full 7-day ring
        // in one response.
        let from = from.max(to - chrono::Duration::hours(24));
        let (from, to) = (Ts::from_utc(from), Ts::from_utc(to));
        // The ring is append-only in arrival order (ts non-decreasing), so
        // binary-search the window bounds instead of scanning every entry.
        let start = w
            .meter_values
            .binary_search_by(|mv| mv.ts.cmp(&from))
            .unwrap_or_else(|i| i);
        w.meter_values
            .iter()
            .skip(start)
            .take_while(|mv| mv.ts <= to)
            .cloned()
            .collect::<Vec<MeterValueSelection>>()
    };
    json(&samples)
}

async fn get_events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> Response {
    let events = {
        let w = st.world.lock().await;
        let from = q.from.unwrap_or(w.last_ts - chrono::Duration::hours(24));
        let from = Ts::from_utc(from);
        // Same as above: the log is append-only, so binary-search the bound.
        let start = w
            .events
            .binary_search_by(|e| e.ts.cmp(&from))
            .unwrap_or_else(|i| i);
        w.events
            .iter()
            .skip(start)
            .cloned()
            .collect::<Vec<EventLogEntry>>()
    };
    json(&events)
}

// ---------------------------------------------------------------------
// plans + session + CP
// ---------------------------------------------------------------------

async fn apply_plan(
    State(st): State<AppState>,
    JsonBody(req): JsonBody<PlanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let plan = evse_plan(&req, Utc::now())?;

    // Apply the plan on the EVSE: the dispatcher runs `evse.set_charging_plan`
    // (BMS-based schedule computation, DB persistence, SetChargingProfile
    // queued for the charging point) and broadcasts a `ChargingPlan`
    // notification, which updates the webserver's world and reaches this page
    // through the SSE stream.
    st.cmd_tx
        .send(CommandToChargingPoint::SetChargingPlan(plan))
        .await
        .map_err(ApiError::internal)?;
    info!(kind = ?req.kind, target = ?req.target_soc, "plan applied");

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn stop_plan(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // Replace the plan in effect on the EVSE with the default (Blocking) plan:
    // the dispatcher runs `evse.set_charging_plan` (inactivates the active
    // schedule, installs the permanent 0 W one) and the resulting
    // `ChargingPlan` notification updates the world and reaches this page
    // through the SSE stream.
    st.cmd_tx
        .send(CommandToChargingPoint::SetChargingPlan(
            ChargingPlan::Blocking,
        ))
        .await
        .map_err(ApiError::internal)?;
    info!("plan stopped");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Set the BMS initial SoC — the same update the CLI performs (local BMS
/// state, no OCPP call) — and re-estimate the world's current SoC from the
/// corrected value, mirroring `web_mock::set_init_soc` so the state catch-up
/// confirms the client's instant preview. When the current SoC is unknown, it
/// is set to the initial SoC (the EVSE does the same on its BMS).
async fn set_init_soc(
    State(st): State<AppState>,
    JsonBody(req): JsonBody<SetInitSocRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !(0.0..=1.0).contains(&req.soc) {
        return Err(ApiError::bad_request("soc must be in [0, 1]"));
    }

    st.cmd_tx
        .send(CommandToChargingPoint::SetInitialSoC(req.soc))
        .await
        .map_err(ApiError::internal)?;

    // world will be updated on server notification if necessary

    info!(soc = req.soc, "initial SoC set");
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn stop_session(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::StopTransaction)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn cp_reboot(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::Reboot)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------
// draft-only simulator controls
// ---------------------------------------------------------------------

async fn set_meter_value_interval(
    State(st): State<AppState>,
    JsonBody(r): JsonBody<SetMeterValueSampleIntervalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::SetMeterValueAlignedInterval(
            r.interval,
        ))
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------

async fn stream(State(st): State<AppState>) -> impl IntoResponse {
    // Each SSE connection gets its own subscriber on the shared notification
    // channel; a lagging client receives `resync` and refetches its snapshot.
    let rx = st.notif_tx.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => {
                let event = Event::default().event(ev.name()).data(ev.to_sse_data());
                Some((Ok::<Event, Infallible>(event), rx))
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "SSE subscriber lagged, sending resync");
                Some((Ok(Event::default().event("resync").data("resync")), rx))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}
