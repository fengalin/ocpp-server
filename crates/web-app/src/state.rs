//! Client-side state + formatting helpers.
//!
//! SoC values are **fractions 0–1** on the wire (full f32 precision; the
//! notification boundary rounds the sample `soc` to 3 decimals); the UI
//! renders them as percentages (`* 100`).

use std::cell::RefCell;
use std::rc::Rc;

use charge_system_common_model::{
    meter_value::MeterValueSelection, model::EventLogEntry, notification::ChargeStatus,
};
use charge_system_web_model::model::{MAX_EVENTS, ServerState};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::dom;

/// SSE link state, shown by the header dot: grey while connecting, green
/// once open, red on error (the browser auto-retries in the background).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Up,
    Down,
}

pub struct ClientState {
    pub server: Option<ServerState>,
    pub samples: Vec<MeterValueSelection>, // visible metrics window (oldest → newest)
    pub events: Vec<EventLogEntry>,        // newest first
    pub link: Link,
    /// Timestamp (ms) of the newest message received from the server — the
    /// header's freshness indicator ("last update").
    pub last_rx_ts: i64,
    /// Selected tab (persisted; the DOM's `active` classes mirror it).
    pub tab: String,
    /// Remaining charging time as last reported by the server:
    /// `(seconds remaining, client-ms instant of the report)`. The countdown
    /// interpolates from this re-baseline between updates.
    pub remaining_at: Option<(i64, i64)>,
    pub window: i64, // metrics window in ms
    pub power: bool,
    pub energy: bool,
    pub voltage: bool,
    pub temp: bool,
    /// Metrics the user has explicitly toggled (on or off). A metric in this
    /// set is never auto-enabled/hidden by the "no data in window" rule.
    pub user_toggled: Vec<&'static str>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            server: None,
            samples: Vec::new(),
            events: Vec::new(),
            link: Link::Connecting,
            last_rx_ts: 0,
            tab: "status".into(),
            remaining_at: None,
            window: DEFAULT_WINDOW_MS,
            power: true,
            energy: false,
            voltage: false,
            temp: true,
            user_toggled: Vec::new(),
        }
    }
}

/// User selections kept across page reloads (selected tab, metrics timeframe,
/// enabled graphs). Webapp-local preferences — the server holds none of them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    pub tab: String,
    pub window: i64,
    pub power: bool,
    pub energy: bool,
    pub voltage: bool,
    pub temp: bool,
}

const PREFS_KEY: &str = "charge-system.web-app.prefs";

/// The toggleable metrics, in `METRICS` display order.
pub const METRIC_KEYS: [&str; 4] = ["power", "energy", "voltage", "temp"];

/// Default metrics window (1 h), also the fallback in `init_metrics_controls`.
pub const DEFAULT_WINDOW_MS: i64 = 3_600_000;

/// The enabled flag of metric `key`, if it is a known metric.
fn metric_flag<'a>(s: &'a mut ClientState, key: &str) -> Option<&'a mut bool> {
    match key {
        "power" => Some(&mut s.power),
        "energy" => Some(&mut s.energy),
        "voltage" => Some(&mut s.voltage),
        "temp" => Some(&mut s.temp),
        _ => None,
    }
}

thread_local! {
    static STATE: Rc<RefCell<ClientState>> = Rc::new(RefCell::new(ClientState::default()));
}

/// Run `f` with a mutable borrow of the global state.
pub fn with_state<R>(f: impl FnOnce(&mut ClientState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub fn set_link(link: Link) {
    with_state(|s| s.link = link);
}

/// Record a server-provided timestamp for the header's freshness indicator.
pub fn bump_rx_ts(ts: i64) {
    with_state(|s| {
        if ts > s.last_rx_ts {
            s.last_rx_ts = ts;
        }
    });
}

/// Append a status entry to the local event log, mirroring the server's
/// retention rule (see `World::push_event`): a charging phase keeps its start
/// entry plus one live entry — a new `Charging` entry appends while the phase
/// holds only its start entry, and replaces the live one once both exist.
/// Live entries are timestamped with the client clock; a refresh replaces the
/// whole log with the server's authoritative history.
pub fn push_event_local(e: EventLogEntry) {
    with_state(|s| {
        let mut leading_charging = 0;
        for ev in s.events.iter() {
            if matches!(ev.status, ChargeStatus::Charging(_)) {
                leading_charging += 1;
            } else {
                break;
            }
        }
        if matches!(&e.status, ChargeStatus::Charging(_)) && leading_charging >= 2 {
            s.events[0] = e; // the newest entry is the live one
            return;
        }
        s.events.insert(0, e);
        s.events.truncate(MAX_EVENTS);
    });
}

/// Load the persisted preferences (`None` on first run or corrupt storage).
pub fn load_prefs() -> Option<Prefs> {
    let Ok(Some(storage)) = dom::window().local_storage() else {
        return None;
    };
    let Ok(Some(raw)) = storage.get_item(PREFS_KEY) else {
        return None;
    };
    serde_json::from_str(&raw).ok()
}

/// Persist the current user selections.
pub fn save_prefs() {
    let prefs = with_state(|s| Prefs {
        tab: s.tab.clone(),
        window: s.window,
        power: s.power,
        energy: s.energy,
        voltage: s.voltage,
        temp: s.temp,
    });
    if let Ok(Some(storage)) = dom::window().local_storage() {
        match serde_json::to_string(&prefs) {
            Ok(json) => {
                if let Err(e) = storage.set_item(PREFS_KEY, &json) {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "could not save prefs: {}",
                        crate::api::js_err(&e)
                    )));
                }
            }
            Err(e) => {
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "could not serialize prefs: {e}"
                )));
            }
        }
    }
}

/// Apply the persisted preferences (if any) on boot: the stored metric
/// selection counts as explicitly user-toggled, so the auto-enable rule never
/// overrides it; an unknown tab falls back to the default.
pub fn apply_prefs(prefs: Option<Prefs>) {
    let Some(prefs) = prefs else {
        return;
    };
    let mut tab = "status".to_string();
    let btns = dom::query_all(".tab");
    for i in 0..btns.length() {
        if let Some(b) = btns.item(i)
            && dom::attr(&b.unchecked_into::<web_sys::Element>(), "data-tab").as_deref()
                == Some(prefs.tab.as_str())
        {
            tab = prefs.tab.clone();
            break;
        }
    }
    with_state(|s| {
        s.tab = tab;
        s.window = prefs.window;
        s.power = prefs.power;
        s.energy = prefs.energy;
        s.voltage = prefs.voltage;
        s.temp = prefs.temp;
        for key in METRIC_KEYS {
            s.user_toggled.push(key);
        }
    });
}

// ---------------------------------------------------------------------
// Formatting (local time from epoch millis)
// ---------------------------------------------------------------------

fn js_date(ms: i64) -> js_sys::Date {
    js_sys::Date::new(&js_sys::Number::from(ms as f64))
}

fn date_opts(fields: &[(&str, &str)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in fields {
        let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), &JsValue::from_str(v));
    }
    o
}

/// Call a JS method on `obj` (via `Reflect.apply`) and return the string
/// result. The locale argument is passed as JS `undefined` on purpose: the
/// web-sys bindings only expose the `&str`-locale overloads, and
/// `to_locale_*_string("")` (an empty-string locale) throws
/// `RangeError: Invalid language tag` in browsers.
fn locale_call(obj: &js_sys::Date, name: &str, opts: Option<&js_sys::Object>) -> Option<String> {
    let m = js_sys::Reflect::get(obj, &JsValue::from_str(name)).ok()?;
    let m: js_sys::Function = m.dyn_into().ok()?;
    let args = js_sys::Array::new();
    if let Some(o) = opts {
        args.push(&JsValue::from(o));
    } else {
        args.push(&JsValue::UNDEFINED);
    }
    js_sys::Reflect::apply(&m, obj, &args)
        .ok()
        .and_then(|s| s.as_string())
}

/// `HH:MM` (24 h, local) — optionally prefixed with `Mon D`.
pub fn fmt_time(ms: i64, with_date: bool) -> String {
    let d = js_date(ms);
    let t = locale_call(&d, "toLocaleTimeString", None).unwrap_or_default();
    if !with_date {
        return t;
    }
    let opts = date_opts(&[("month", "short"), ("day", "numeric")]);
    let date = locale_call(&d, "toLocaleDateString", Some(&opts)).unwrap_or_default();
    format!("{date} {t}")
}

/// `HH:MM` (24 h, local, no seconds) — optionally prefixed with the short
/// weekday (`Tue 14:32`). Used by the chart scrub popup and x-axis scale,
/// where the window width decides whether a weekday is shown at all. The time
/// is built from the `Date` parts (not `toLocaleTimeString`): the locale
/// default time string includes seconds, which overloads the chart x axis on
/// long windows.
pub fn fmt_chart_time(ms: i64, with_weekday: bool) -> String {
    let d = js_date(ms);
    let p = |n: i32| format!("{:02}", n);
    let t = format!("{}:{}", p(d.get_hours() as i32), p(d.get_minutes() as i32));
    if !with_weekday {
        return t;
    }
    let opts = date_opts(&[("weekday", "short")]);
    let wd = locale_call(&d, "toLocaleDateString", Some(&opts)).unwrap_or_default();
    format!("{wd} {t}")
}

/// Is metric `key` currently enabled? (The toggles are stored as plain
/// bools on [`ClientState`].)
pub fn is_enabled(key: &str) -> bool {
    with_state(|s| metric_flag(s, key).copied().unwrap_or(false))
}

/// Does metric `key` have at least one data point in the current sample
/// window?
pub fn has_data(key: &str) -> bool {
    with_state(|s| s.samples.iter().any(|m| metric_value_of(key, m).is_some()))
}

/// The display value of metric `key` in a sample (kW / kWh / V / °C), or
/// `None` when the sample carries no such field. Single implementation —
/// `plot::MetricDef::value_of` delegates here.
pub fn metric_value_of(key: &str, m: &MeterValueSelection) -> Option<f64> {
    match key {
        "power" => m.power.map(|w| w as f64 / 1000.0),
        "temp" => m.temperature.map(|t| t as f64),
        "voltage" => m.voltage.map(|v| v as f64),
        "energy" => m.energy.map(|w| w as f64 / 1000.0),
        _ => None,
    }
}

/// Record that the user explicitly toggled metric `key` (on or off). A
/// user-toggled metric is exempt from the auto-enable/auto-hide rule.
pub fn mark_user_toggled(key: &str) {
    let Some(k) = METRIC_KEYS.into_iter().find(|k| *k == key) else {
        return;
    };
    with_state(|s| {
        if !s.user_toggled.contains(&k) {
            s.user_toggled.push(k);
        }
    })
}

/// Whether the user has explicitly toggled metric `key` (on or off).
// (`contains` would need a `&&'static str`; the `any` form accepts any `&str`.)
#[allow(clippy::manual_contains)]
pub fn user_toggled(key: &str) -> bool {
    with_state(|s| s.user_toggled.iter().any(|&k| k == key))
}

/// Set metric `key` enabled/disabled (used by the auto-enable rule).
pub fn set_enabled(key: &str, on: bool) {
    with_state(|s| {
        if let Some(flag) = metric_flag(s, key) {
            *flag = on;
        }
    })
}

/// Toggle metric `key`, returning the new value.
pub fn toggle(key: &str) -> bool {
    with_state(|s| {
        metric_flag(s, key)
            .map(|flag| {
                *flag = !*flag;
                *flag
            })
            .unwrap_or(false)
    })
}

/// Duration from seconds: `H h MM min` or `M min`.
pub fn fmt_dur(secs: u64) -> String {
    let m = ((secs as f64 / 60.0).round()) as i64;
    let h = m / 60;
    if h > 0 {
        format!("{} h {:02} min", h, m % 60)
    } else {
        format!("{} min", m)
    }
}

/// Format an epoch-millis timestamp as a `date` input value
/// (`YYYY-MM-DD`, local time).
pub fn to_local_date(ms: i64) -> String {
    let d = js_date(ms);
    let p = |n: i32| format!("{:02}", n);
    format!(
        "{:04}-{}-{}",
        d.get_full_year(),
        p((d.get_month() + 1) as i32),
        p(d.get_date() as i32),
    )
}

/// Format an epoch-millis timestamp as a `time` input value
/// (`HH:MM`, local time).
pub fn to_local_time(ms: i64) -> String {
    let d = js_date(ms);
    let p = |n: i32| format!("{:02}", n);
    format!("{}:{}", p(d.get_hours() as i32), p(d.get_minutes() as i32))
}

/// `Date.parse(s)` as epoch millis, or `None` when unparseable (NaN).
fn date_parse_ms(s: &str) -> Option<i64> {
    let ms = js_sys::Date::parse(s);
    if js_sys::Number::is_nan(&JsValue::from_f64(ms)) {
        None
    } else {
        Some(ms as i64)
    }
}

/// Parse a `datetime-local` input value (local) to epoch millis.
pub fn from_local_input_value(s: &str) -> Option<i64> {
    date_parse_ms(s)
}

/// Combine a `date` value (`YYYY-MM-DD`) and a `time` value (`HH:MM`), both
/// local, into epoch millis. Returns `None` when either part is missing or
/// unparseable.
pub fn from_local_date_time(date: &str, time: &str) -> Option<i64> {
    if date.is_empty() || time.is_empty() {
        return None;
    }
    from_local_input_value(&format!("{date}T{time}"))
}

/// ISO-8601 (UTC) string from epoch millis — for the `/api/metrics?from&to`
/// query params, which the server parses as UTC.
pub fn to_iso_utc(ms: i64) -> String {
    String::from(js_date(ms).to_iso_string())
}

/// Parse an ISO-8601 string to epoch millis (for reading query-param echoes).
pub fn from_iso(s: &str) -> Option<i64> {
    date_parse_ms(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dur_formats() {
        assert_eq!(fmt_dur(59), "1 min");
        assert_eq!(fmt_dur(60), "1 min");
        assert_eq!(fmt_dur(3600), "1 h 00 min");
        assert_eq!(fmt_dur(3_600 * 6 + 120), "6 h 02 min");
    }

    #[test]
    fn default_state() {
        let s = ClientState::default();
        assert_eq!(s.window, 3_600_000);
        assert!(s.power);
        assert!(s.temp);
        assert!(!s.energy);
        assert!(!s.voltage);
        assert_eq!(s.link, Link::Connecting);
        assert_eq!(s.last_rx_ts, 0);
        assert_eq!(s.tab, "status");
        assert_eq!(s.remaining_at, None);
        assert!(s.samples.is_empty());
        assert!(s.events.is_empty());
    }

    #[test]
    fn prefs_serialize_round_trip() {
        let p = Prefs {
            tab: "metrics".into(),
            window: 604_800_000,
            power: true,
            energy: true,
            voltage: false,
            temp: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap(), p);
    }

    fn entry(ts: i64, status: ChargeStatus) -> EventLogEntry {
        EventLogEntry {
            ts: charge_system_common_model::model::Ts(ts),
            status,
        }
    }

    fn charging(soc: f32) -> ChargeStatus {
        ChargeStatus::Charging(charge_system_common_model::notification::ChargeProgress {
            soc,
            remaining_time: 100,
        })
    }

    #[test]
    fn local_event_log_mirrors_server_dedup() {
        // Newest first: push a charging phase with three progress updates.
        with_state(|s| {
            s.events.clear();
        });
        push_event_local(entry(1, ChargeStatus::Available));
        push_event_local(entry(2, charging(0.2)));
        push_event_local(entry(3, charging(0.4)));
        push_event_local(entry(4, charging(0.6)));

        let list = with_state(|s| s.events.clone());
        assert_eq!(list.len(), 3);
        // Newest first: live entry (latest) first, then the frozen start
        // entry, then the rest of the history.
        assert_eq!(list[0].ts.0, 4);
        assert_eq!(list[1].ts.0, 2);
        assert_eq!(list[2].ts.0, 1);
    }

    #[test]
    fn local_event_log_caps_at_max_events() {
        with_state(|s| {
            s.events.clear();
        });
        for i in 0..MAX_EVENTS + 5 {
            let status = if i % 2 == 0 {
                ChargeStatus::Available
            } else {
                ChargeStatus::Preparing
            };
            push_event_local(entry(i as i64, status));
        }
        assert_eq!(with_state(|s| s.events.len()), MAX_EVENTS);
    }
}
