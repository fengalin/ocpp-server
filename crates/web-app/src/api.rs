//! JSON REST + SSE client.
//!
//! Every request/response body is JSON (`application/json`). SSE payloads are
//! JSON strings — the mirror of the server's `Notification::to_sse_data`.

use charge_system_common_model::{
    bms::Bms,
    meter_value::MeterValueSelection,
    model::EventLogEntry,
    notification::{ChargeStatus, Notification},
    plan_model::AppliedChargingPlan,
};
use charge_system_web_model::model::{PlanRequest, ServerState};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{EventSource, MessageEvent, window};

/// Encode a serializable request body to a JSON string for the `fetch` body.
///
/// Deliberately `&impl Serialize` (not `&dyn Serialize`): the body is consumed
/// by `to_string` and only the string reaches `fetch`, so a trait object buys
/// nothing here while adding a vtable hop per call.
fn jsval(v: &impl serde::Serialize) -> JsValue {
    let s = serde_json::to_string(v).expect("request serializes");
    JsValue::from_str(&s)
}

/// Decode a JSON response body.
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

async fn get_text(url: &str) -> Result<String, String> {
    let w = window().ok_or("no window")?;
    let res = JsFuture::from(w.fetch_with_str(url))
        .await
        .map_err(|e| format!("fetch {}: {}", url, js_err(&e)))?;
    let res: web_sys::Response = res
        .dyn_into()
        .map_err(|_| format!("fetch {}: not a response", url))?;
    if !res.ok() {
        return Err(err_text(&res).await);
    }
    read_text(res).await
}

/// POST with a JSON body (the whole front-end data plane is JSON).
async fn post_json(url: &str, body: &impl serde::Serialize) -> Result<String, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&jsval(body));
    let headers = web_sys::Headers::new().map_err(|e| js_err(&e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| js_err(&e))?;
    opts.set_headers(&headers);
    post(url, opts).await
}

/// POST with no request body (the server's no-arg endpoints) — a JSON `null`
/// body would just be noise.
async fn post_empty(url: &str) -> Result<String, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    post(url, opts).await
}

/// Shared `fetch` transport behind the `post_json` / `post_empty` variants.
async fn post(url: &str, opts: web_sys::RequestInit) -> Result<String, String> {
    let w = window().ok_or("no window")?;
    let res = JsFuture::from(w.fetch_with_str_and_init(url, &opts))
        .await
        .map_err(|e| format!("fetch {}: {}", url, js_err(&e)))?;
    let res: web_sys::Response = res.dyn_into().map_err(|_| "not a response".to_string())?;
    if !res.ok() {
        return Err(err_text(&res).await);
    }
    read_text(res).await
}

async fn err_text(res: &web_sys::Response) -> String {
    match res.text() {
        Ok(p) => match JsFuture::from(p).await {
            Ok(v) => format!("{} {}", res.status(), v.as_string().unwrap_or_default()),
            Err(e) => format!("{} (read error: {})", res.status(), js_err(&e)),
        },
        Err(e) => format!("{} (no body: {})", res.status(), js_err(&e)),
    }
}

async fn read_text(res: web_sys::Response) -> Result<String, String> {
    let p = res.text().map_err(|e| js_err(&e))?;
    let v = JsFuture::from(p).await.map_err(|e| js_err(&e))?;
    Ok(v.as_string().unwrap_or_default())
}

/// `JsValue` (a rejected promise / JS error) does not implement `Display`.
pub(crate) fn js_err(e: &JsValue) -> String {
    match e.as_string() {
        Some(s) => s,
        None => js_sys::JSON::stringify(e)
            .ok()
            .map(String::from)
            .unwrap_or_else(|| "js error".to_string()),
    }
}

pub async fn get_state() -> Result<ServerState, String> {
    decode(&get_text("/api/state").await?)
}

/// `from`/`to` are ISO-8601 strings (URL query params, not JSON bodies).
pub async fn get_samples(from_iso: &str, to_iso: &str) -> Result<Vec<MeterValueSelection>, String> {
    let url = format!(
        "/api/metrics?from={}&to={}",
        urlenc(from_iso),
        urlenc(to_iso)
    );
    decode(&get_text(&url).await?)
}

pub async fn get_events() -> Result<Vec<EventLogEntry>, String> {
    decode(&get_text("/api/events").await?)
}

/// The server answers `{ "ok": true }`; the applied plan itself reaches the
/// UI through the `charging_plan` SSE event.
pub async fn apply_plan(body: &PlanRequest) -> Result<(), String> {
    post_json("/api/plans/apply", body).await?;
    Ok(())
}

#[derive(Clone, Copy, serde::Serialize)]
pub struct SetInitSoc {
    /// Last known state of charge at session start (fraction 0–1).
    pub soc: f32,
}

/// Push the corrected initial SoC to the server. It updates the BMS the same
/// way the CLI does and re-estimates the current SoC, so the state catch-up
/// confirms the client-side instant preview.
pub async fn set_init_soc(soc: f32) -> Result<(), String> {
    post_json("/api/bms/init_soc", &SetInitSoc { soc })
        .await
        .map(|_| ())
}

#[derive(Clone, Copy, serde::Serialize)]
struct SetInterval {
    interval: u16,
}

pub async fn set_meter_value_interval(interval: u16) -> Result<(), String> {
    post_json(
        "/api/cp/set_meter_value_interval",
        &SetInterval { interval },
    )
    .await
    .map(|_| ())
}

pub async fn stop_plan() -> Result<(), String> {
    post_empty("/api/plans/stop").await.map(|_| ())
}

pub async fn stop_session() -> Result<(), String> {
    post_empty("/api/session/stop").await.map(|_| ())
}

pub async fn reboot_cp() -> Result<(), String> {
    post_empty("/api/cp/reboot").await.map(|_| ())
}

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s)
        .as_string()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------

pub struct StreamHandlers {
    pub on_bms_update: Box<dyn Fn(Bms)>,
    pub on_meter_values: Box<dyn Fn(MeterValueSelection)>,
    pub on_status: Box<dyn Fn(ChargeStatus)>,
    pub on_charging_plan: Box<dyn Fn(AppliedChargingPlan)>,
    pub on_open: Box<dyn Fn()>,
}

/// Connect the SSE stream. On (re)open the caller does a full catch-up.
///
/// Returns `Err` when the browser has no `EventSource` support or a listener
/// cannot be registered — the caller logs it and keeps running (the REST
/// catch-up still works without the live stream).
pub fn connect_stream(h: StreamHandlers) -> Result<EventSource, String> {
    let es: EventSource =
        EventSource::new("/api/stream").map_err(|e| format!("EventSource: {}", js_err(&e)))?;

    let on_open = std::rc::Rc::new(h.on_open);
    let on_open_cb = dom_closure({
        let on_open = on_open.clone();
        move |_e| {
            super::state::set_link(super::state::Link::Up);
            // No periodic tick re-renders: show the green dot immediately,
            // without waiting for the catch-up refresh to land.
            super::ui::render::render_header();
            (on_open)();
        }
    });
    es.set_onopen(Some(&on_open_cb));

    let on_error_cb = dom_closure(move |_e| {
        super::state::set_link(super::state::Link::Down);
        // Same: the red dot must appear even while no refresh is running.
        super::ui::render::render_header();
    });
    es.set_onerror(Some(&on_error_cb));

    // Server lag: the client must do a full catch-up.
    let resync = dom_closure(move |_e| (on_open)());
    es.add_event_listener_with_callback("resync", &resync)
        .map_err(|e| format!("add resync listener: {}", js_err(&e)))?;

    on_notification(
        &es,
        "meter_values",
        |n| match n {
            Notification::MeterValues(m) => Some(m),
            _ => None,
        },
        h.on_meter_values,
    )?;
    on_notification(
        &es,
        "bms_update",
        |n| match n {
            Notification::BmsUpdate(b) => Some(b),
            _ => None,
        },
        h.on_bms_update,
    )?;
    on_notification(
        &es,
        "status",
        |n| match n {
            Notification::Status(s) => Some(s),
            _ => None,
        },
        h.on_status,
    )?;
    // Any applied or cleared plan refreshes the whole UI.
    on_notification(
        &es,
        "charging_plan",
        |n| match n {
            Notification::ChargingPlan(p) => Some(p),
            _ => None,
        },
        h.on_charging_plan,
    )?;

    // The EventSource keeps each registered callback alive while the listener
    // is attached, so we can let the local `Function` values drop here.
    Ok(es)
}

/// A panic-guarded `js_sys::Function` for the raw `Closure` sites (the
/// web-sys `set_on*` / `add_event_listener` calls below).
fn dom_closure(f: impl FnMut(&web_sys::Event) + 'static) -> js_sys::Function {
    crate::dom::closure(f)
}

/// Register a handler for one SSE event name: decode the `data:` JSON into a
/// `Notification`, extract this event's variant, and call `h` with it.
fn on_notification<T: 'static>(
    es: &EventSource,
    name: &str,
    extract: fn(Notification) -> Option<T>,
    h: impl Fn(T) + 'static,
) -> Result<(), String> {
    let cb = dom_closure(move |e: &web_sys::Event| {
        let e = e.unchecked_ref::<MessageEvent>();
        let Some(data) = e.data().as_string() else {
            return;
        };
        if let Some(n) = decode_sse(&data)
            && let Some(v) = extract(n)
        {
            h(v);
        }
    });
    es.add_event_listener_with_callback(name, &cb)
        .map_err(|e| format!("add {name} listener: {}", js_err(&e)))
}

fn decode_sse(json: &str) -> Option<Notification> {
    match serde_json::from_str(json) {
        Ok(n) => Some(n),
        Err(e) => {
            // A malformed SSE payload must not pass silently.
            web_sys::console::warn_1(&JsValue::from_str(&format!(
                "dropping malformed SSE payload: {e}"
            )));
            None
        }
    }
}
