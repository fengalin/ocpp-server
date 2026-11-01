use js_sys::Date;
use wasm_bindgen::JsValue;

use charge_system_common_model::{
    bms::{self, Bms},
    model::EventLogEntry,
    notification::ChargeStatus,
    plan_model::{AppliedChargingPlan, ChargingPlan},
};

use crate::ui::{plan, status_color, status_name};
use crate::{dom, plot, state};

/// A charging session is in progress when the CP is preparing, charging or
/// suspended by the EVSE.
pub(crate) fn session_active(s: &ChargeStatus) -> bool {
    matches!(
        s,
        ChargeStatus::Preparing | ChargeStatus::Charging(_) | ChargeStatus::SuspendedEvse(_)
    )
}

/// Name of the active tab. The source of truth is `state.tab` (kept by
/// `tabs::activate_tab`); the DOM's `active` classes mirror it.
pub(crate) fn active_tab() -> String {
    state::with_state(|s| s.tab.clone())
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn js_date(ms: i64) -> Date {
    Date::new(&JsValue::from_f64(ms as f64))
}

/// Epoch millis of the local midnight of the day containing `ms`.
fn local_midnight(ms: i64) -> i64 {
    let d = js_date(ms);
    let _ = d.set_hours(0);
    let _ = d.set_minutes(0);
    let _ = d.set_seconds(0);
    let _ = d.set_milliseconds(0);
    d.get_time() as i64
}

/// Whole days from local today to the date of `ms` (negative = past).
fn day_diff_from_today(ms: i64) -> i64 {
    // The midnight difference spans at most one DST shift, so round.
    let diff = local_midnight(ms) - local_midnight(Date::now() as i64);
    (diff as f64 / 86_400_000.0).round() as i64
}

fn same_local_day(a: i64, b: i64) -> bool {
    local_midnight(a) == local_midnight(b)
}

/// "HH:MM" for today, "ddd HH:MM" for tomorrow, "ddd d HH:MM" further out.
fn fmt_when(ms: i64) -> String {
    let time = state::to_local_time(ms);
    match day_diff_from_today(ms) {
        0 => time,
        1 => format!("{} {time}", WEEKDAYS[js_date(ms).get_day() as usize]),
        _ => {
            let d = js_date(ms);
            format!("{} {} {time}", WEEKDAYS[d.get_day() as usize], d.get_date())
        }
    }
}

/// "{start} → {end}" for a plan's schedule: the start is formatted relative
/// to today; the end shows only the time when it ends on the same day the
/// plan starts (never repeating a day), otherwise day + time.
pub(crate) fn fmt_range(start_ms: i64, end_ms: i64) -> String {
    let end = if same_local_day(start_ms, end_ms) {
        state::to_local_time(end_ms)
    } else {
        fmt_when(end_ms)
    };
    format!("{} → {}", fmt_when(start_ms), end)
}

pub(crate) fn plan_name(p: &ChargingPlan) -> &'static str {
    match p {
        ChargingPlan::Asap { .. } => "ASAP",
        ChargingPlan::Deadline { .. } => "deadline",
        ChargingPlan::OffPeakToday { .. } => "off-peak (today)",
        ChargingPlan::OffPeakTomorrow { .. } => "off-peak (tomorrow)",
        ChargingPlan::Blocking => "blocking",
        ChargingPlan::Unlimited => "unlimited",
    }
}

fn plan_card_html(applied: &AppliedChargingPlan, bms: &Bms, remaining_id: &str) -> String {
    let plan = &applied.plan;
    // Blocking has no schedule, target or remaining time — the card body is
    // just the plan name. (Unlimited never reaches here: it hides the whole
    // card upstream by mapping to `None`.)
    if matches!(plan, ChargingPlan::Blocking) {
        return format!(
            "<div class=\"kv\"><span>Plan</span><b>{}</b></div>",
            plan_name(plan),
        );
    }
    let schedule_txt = match (applied.periods.first(), applied.end()) {
        (Some(first), Some(end)) => fmt_range(first.start_at.0, end.0),
        _ => "—".to_string(),
    };
    let target = plan.target_soc();
    let target_txt = target
        .map(|t| format!("{:.0} %", t * 100.0))
        .unwrap_or_else(|| "—".to_string());
    let remaining = match (target, bms.current_soc.absolute()) {
        (Some(t), Some(s)) => {
            let pct = ((t - s) * 100.0).max(0.0);
            match bms.target_soc_to_energy(bms::SoC::new_absolute(t)) {
                Some(w) => format!("{pct:.1} % · {:.1} kWh", w as f64 / 1000.0),
                None => format!("{pct:.1} %"),
            }
        }
        _ => "—".to_string(),
    };
    format!(
        "<div class=\"kv\"><span>Plan</span><b>{}</b></div>\
         <div class=\"kv\"><span>Schedule</span><b>{schedule_txt}</b></div>\
         <div class=\"kv\"><span>Target</span><b>{target_txt}</b></div>\
         <div class=\"kv\"><span>Remaining</span><b>{remaining}</b></div>\
         <div class=\"kv\"><span>Remaining time</span><b id=\"{remaining_id}\">—</b></div>",
        plan_name(plan),
    )
}

/// The header (clock + link dot) — rendered both by [`render`] and directly
/// on SSE open/error, since no periodic tick re-renders the page anymore.
pub(crate) fn render_header() {
    let (txt, cls, title) = state::with_state(|s| {
        // The clock shows the last message received from the server — a
        // freshness indicator, not a ticking wall clock.
        let txt = if s.last_rx_ts > 0 {
            state::fmt_time(s.last_rx_ts, true)
        } else {
            "connecting…".to_string()
        };
        let (cls, title) = match s.link {
            state::Link::Connecting => ("conn", "Connecting…"),
            state::Link::Up => ("conn ok", "Live"),
            state::Link::Down => ("conn bad", "Disconnected — retrying"),
        };
        (txt, cls, title)
    });
    let clock = dom::el("clock");
    dom::set_text(&clock, &txt);
    let conn = dom::el_html("conn");
    conn.set_class_name(cls);
    conn.set_title(title);
}

fn render_status() {
    render_badge();
    render_soc();
    render_tiles();
    render_plan_card();
}

fn render_badge() {
    // Read only what is rendered, inside a scoped borrow (no whole-state clone).
    let Some((name, color, clickable)) = state::with_state(|s| {
        s.server.as_ref().map(|srv| {
            (
                status_name(&srv.status),
                status_color(&srv.status),
                session_active(&srv.status),
            )
        })
    }) else {
        return;
    };
    let badge = dom::el_html("status-badge");
    // The badge also hosts the stop button — update only the text span.
    dom::set_text(&dom::el("status-badge-text"), name);
    dom::set_style(&badge, "color", color);
    dom::set_style(&badge, "border-color", color);
    dom::set_style(&badge, "background", &format!("{color}1f"));
    dom::set_style(
        &badge,
        "cursor",
        if clickable { "pointer" } else { "default" },
    );
    badge.set_title(if clickable {
        "Click to stop session"
    } else {
        ""
    });
    if clickable {
        dom::add_class(&badge, "clickable");
    } else {
        dom::remove_class(&badge, "clickable");
    }
    // Stop button, hosted in the status badge (shown when a session can stop).
    let stop_btn = dom::el("stop-session-btn");
    if clickable {
        dom::remove_class(&stop_btn, "hidden");
    } else {
        dom::add_class(&stop_btn, "hidden");
    }
}

fn render_soc() {
    let Some((soc, target)) = state::with_state(|s| {
        s.server
            .as_ref()
            .map(|srv| (srv.bms.current_soc.absolute(), srv.bms.target_soc))
    }) else {
        return;
    };
    dom::set_text(
        &dom::el("soc-text"),
        &soc.map(|v| format!("{:.1} %", v * 100.0))
            .unwrap_or_else(|| "—".to_string()),
    );
    let soc_fill = dom::el("soc-fill");
    if let Some(soc) = soc {
        dom::remove_class(&soc_fill, "hidden");
        dom::set_style(
            &soc_fill,
            "width",
            &format!("{}%", (soc * 100.0).clamp(0.0, 100.0)),
        );
    } else {
        dom::add_class(&soc_fill, "hidden");
    }

    let mark = dom::el("soc-target-mark");
    let soc_target = dom::el("soc-target");
    if let Some(target) = target {
        dom::set_style(
            &mark,
            "left",
            &format!("{}%", (target * 100.0).clamp(0.0, 100.0)),
        );
        dom::remove_class(&mark, "hidden");
        dom::set_text(&soc_target, &format!("target {:.0} %", target * 100.0));
    } else {
        dom::add_class(&mark, "hidden");
        dom::set_text(&soc_target, "");
    }
}

fn render_tiles() {
    let tiles: Option<Vec<(&str, String)>> = state::with_state(|s| {
        let srv = s.server.as_ref()?;
        let last = s.samples.last();
        let charging = session_active(&srv.status) && last.and_then(|m| m.power).is_some();
        let power_txt = match (charging, last.and_then(|m| m.power)) {
            (true, Some(p)) => format!("{:.3} kW", p as f64 / 1000.0),
            _ => "—".to_string(),
        };
        // Session energy comes from the BMS copy (current − initial):
        // `on_meter_values` keeps `current_energy` fresh from samples carrying
        // an energy reading.
        let session_energy = match (srv.bms.initial_energy, srv.bms.current_energy) {
            (Some(initial_energy), Some(energy)) => Some(energy.saturating_sub(initial_energy)),
            _ => None,
        };
        Some(vec![
            ("Power", power_txt),
            (
                "Session energy",
                session_energy
                    .map(|e| format!("{:.1} kWh", e as f64 / 1000.0))
                    .unwrap_or_else(|| "—".to_string()),
            ),
            (
                "Voltage",
                last.and_then(|m| m.voltage)
                    .map(|v| format!("{v} V"))
                    .unwrap_or_else(|| "—".to_string()),
            ),
            (
                "CP temperature",
                last.and_then(|m| m.temperature)
                    .map(|t| format!("{t} °C"))
                    .unwrap_or_else(|| "—".to_string()),
            ),
        ])
    });
    let Some(tiles) = tiles else {
        return;
    };
    let mut tiles_html = String::new();
    for (l, v) in &tiles {
        tiles_html.push_str(&format!(
            "<div class=\"tile\"><div class=\"tile-v\">{v}</div><div class=\"tile-l\">{l}</div></div>"
        ));
    }
    dom::set_inner_html(&dom::el("tiles"), &tiles_html);
}

fn render_plan_card() {
    // Active-plan card (status tab only — the plan tab mirrors the applied
    // plan in its form fields and preview instead). The header + stop button
    // are static elements; only the body is re-rendered.
    let card = state::with_state(|s| {
        let srv = s.server.as_ref()?;
        let applied = srv.charging_plan.as_ref()?;
        // The stop button only makes sense for plans that can actually be
        // stopped (deadline / ASAP): Blocking is already the default,
        // and a missing plan (unlimited) hides the whole card.
        let stoppable = matches!(
            &applied.plan,
            ChargingPlan::Deadline { .. } | ChargingPlan::Asap { .. }
        );
        Some((
            plan_card_html(applied, &srv.bms, "plan-remaining"),
            stoppable,
        ))
    });
    let pc = dom::el("plan-card");
    let stop_btn = dom::el("plan-stop-btn");
    match card {
        Some((body, stoppable)) => {
            dom::remove_class(&pc, "hidden");
            dom::set_inner_html(&dom::el("plan-card-body"), &body);
            if stoppable {
                dom::remove_class(&stop_btn, "hidden");
            } else {
                dom::add_class(&stop_btn, "hidden");
            }
        }
        None => {
            dom::add_class(&pc, "hidden");
        }
    }
}

fn render_log() {
    let events: Vec<EventLogEntry> = state::with_state(|s| s.events.clone());
    let html = if events.is_empty() {
        "<p class='muted'>No events yet.</p>".to_string()
    } else {
        events
            .iter()
            .map(|e| {
                let c = status_color(&e.status);
                format!(
                    "<div class=\"log-entry\"><span class=\"log-time\">{time}</span>\
                     <span class=\"status-chip\" style=\"color:{c};border-color:{c}55\">{name}</span></div>",
                    time = state::fmt_time(e.ts.0, true),
                    name = status_name(&e.status),
                )
            })
            .collect()
    };
    dom::set_inner_html(&dom::el("log-list"), &html);
}

/// Full re-render of every tab's dynamic content (called after refreshes,
/// SSE events and user actions).
pub fn render() {
    render_header();
    render_status();
    render_log();
    plan::ensure_deadline_default();
    plan::init_soc_sync();
    plan::target_sync();
    plan::plan_form_sync();
    plan::plan_schedule_preview();
    if active_tab() == "metrics" {
        plot::sync();
    }
    // Keep the countdown (the app's only timer) consistent with the just
    // rendered plan cards.
    plan::countdown_sync();
}
