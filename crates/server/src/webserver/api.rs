//! HTTP API: REST endpoints, SSE stream and static file serving.

use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures::stream;
use serde::Deserialize;
use tower_http::services::ServeDir;
use tracing::info;

use crate::{measurements::MeterValueSelection, ocpp::CommandToChargingPoint};

use super::model::{
    EventLogEntry, Plan, PlanKind, PlanPreview, PlanRequest, PlanState, ServerState,
};
use super::plan;
use super::state::AppState;

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

    pub fn conflict(msg: impl ToString) -> Self {
        Self {
            status: StatusCode::CONFLICT,
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
struct SetMeterValueSampleIntervalRequest {
    interval: u16,
}

fn validate_plan(req: &PlanRequest, now: DateTime<Utc>) -> Result<(), ApiError> {
    if req.target_soc <= 0.0 || req.target_soc > 100.0 {
        return Err(ApiError::bad_request("target_soc must be in (0, 100]"));
    }
    if req.kind == PlanKind::Deadline {
        let d = req
            .deadline
            .ok_or_else(|| ApiError::bad_request("deadline is required for deadline plans"))?;
        if d <= now {
            return Err(ApiError::bad_request("deadline must be in the future"));
        }
        if d > now + chrono::Duration::hours(48) {
            return Err(ApiError::bad_request(
                "deadline must be within the next 48 h",
            ));
        }
    }
    Ok(())
}

pub fn router(st: AppState) -> Router {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/samples", get(get_samples))
        .route("/api/events", get(get_events))
        .route("/api/plans/preview", post(preview_plan))
        .route("/api/plans", post(create_plan))
        .route("/api/plans/stop", post(stop_plan))
        .route("/api/session/stop", post(stop_session))
        .route(
            "/api/cp/set_meter_value_interval",
            post(set_meter_value_interval),
        )
        .route("/api/cp/reboot", post(cp_reboot))
        .route("/api/stream", get(stream))
        .fallback_service(ServeDir::new("static"))
        .with_state(st)
}

// ---------------------------------------------------------------------
// state + history
// ---------------------------------------------------------------------

async fn get_state(State(st): State<AppState>) -> Json<ServerState> {
    Json(st.world.lock().await.server_state())
}

async fn get_samples(
    State(st): State<AppState>,
    Query(q): Query<MeterValuesQuery>,
) -> Json<Vec<MeterValueSelection>> {
    let w = st.world.lock().await;
    let from = q.from.unwrap_or(w.last_ts - chrono::Duration::hours(1));
    let to = q.to.unwrap_or(w.last_ts);
    // Clamp the window to 24 h so a client can't request the full 7-day ring
    // in one response.
    let from = from.max(to - chrono::Duration::hours(24));
    Json(
        w.meter_values
            .iter()
            .filter(|mv| mv.ts >= from && mv.ts <= to)
            .cloned()
            .collect(),
    )
}

async fn get_events(
    State(st): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Json<Vec<EventLogEntry>> {
    let w = st.world.lock().await;
    let from = q.from.unwrap_or(w.last_ts - chrono::Duration::hours(24));
    Json(w.events.iter().filter(|e| e.ts >= from).cloned().collect())
}

// ---------------------------------------------------------------------
// plans + session + CP
// ---------------------------------------------------------------------

async fn preview_plan(
    State(st): State<AppState>,
    Json(req): Json<PlanRequest>,
) -> Result<Json<PlanPreview>, ApiError> {
    let w = st.world.lock().await;
    validate_plan(&req, w.last_ts)?;
    Ok(Json(plan::compute(
        req.kind,
        w.last_ts,
        w.cur_soc
            .ok_or_else(|| ApiError::conflict("current SoC is unknown"))?,
        req.target_soc,
        req.deadline,
        &w.ch,
    )))
}

async fn create_plan(
    State(st): State<AppState>,
    Json(req): Json<PlanRequest>,
) -> Result<Json<Plan>, ApiError> {
    let mut w = st.world.lock().await;
    validate_plan(&req, w.last_ts)?;
    if w.plan
        .as_ref()
        .is_some_and(|p| p.state == PlanState::Active)
    {
        return Err(ApiError::conflict(
            "a plan is already active; stop it first",
        ));
    }
    let preview = plan::compute(
        req.kind,
        w.last_ts,
        w.cur_soc
            .ok_or_else(|| ApiError::conflict("current SoC is unknown"))?,
        req.target_soc,
        req.deadline,
        &w.ch,
    );
    let created = w.install_plan(req.kind, req.target_soc, req.deadline, preview);
    // let _ = st.front_end_tx.send(StreamEvent::Plan);
    info!(kind = ?req.kind, target = req.target_soc, "plan created");

    created.map(|c| Json(c))
}

async fn stop_plan(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let mut w = st.world.lock().await;
    let mut evs = Vec::new();
    if !w.stop_plan(&mut evs) {
        return Err(ApiError::conflict("no active plan"));
    }
    // for ev in evs {
    //     let _ = st.front_end_tx.send(ev);
    // }
    info!("plan stopped");
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn stop_session(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::StopTransaction)
        .await
        .map_err(|err| ApiError::internal(err))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn cp_reboot(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::Reboot)
        .await
        .map_err(|err| ApiError::internal(err))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------
// draft-only simulator controls
// ---------------------------------------------------------------------

async fn set_meter_value_interval(
    State(st): State<AppState>,
    Json(r): Json<SetMeterValueSampleIntervalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    st.cmd_tx
        .send(CommandToChargingPoint::SetMeterValueAlignedInterval(
            r.interval,
        ))
        .await
        .map_err(|err| ApiError::internal(err))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------

async fn stream(State(st): State<AppState>) -> impl IntoResponse {
    let rx = st.front_end_tx.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => {
                let event = Event::default()
                    .event(ev.name())
                    .data(serde_json::to_string(&ev).unwrap());
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
