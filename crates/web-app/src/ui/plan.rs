//! The plan form (ported from plan.ts): the debounced client-side preview,
//! apply, and the init-SoC widget (now living on the status tab).

use std::rc::Rc;

use chrono::Timelike;

use charge_system_common_model::{
    bms,
    model::Ts,
    notification::ChargeStatus,
    plan as plan_math,
    plan_model::{ChargingPlan, PlanKind, PlanPreview},
};
use charge_system_web_model::model::PlanRequest;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlButtonElement, HtmlInputElement};

use super::actions::confirm_show;
use super::catchup::console_error;
use super::render::{fmt_range, plan_name, session_active};
use crate::state::ClientState;
use crate::ui::{now_ms, now_or_server_ts, on, with_ui, with_ui_ref};
use crate::{api, dom, state};

/// The target SoC the form starts from until `bms.target_soc` (the source of
/// truth) is known or the user edits the field.
pub(crate) const DEFAULT_TARGET_PCT: u32 = 55;

/// Default deadline: "now + 4 h" (both the `ensure_deadline_default` seed and
/// the fallback when mirroring a plan's time of day).
const DEFAULT_DEADLINE_OFFSET_MS: i64 = 4 * 3_600_000;

/// Debounce of the client-side plan preview (ms).
const PREVIEW_DEBOUNCE_MS: i32 = 400;

/// Seed the target-SoC widget from `bms.target_soc` (the single source of
/// truth). Called on init and whenever a `BmsUpdate` arrives: a user's local
/// edit persists across renders until the server overrides it.
///
/// The default only applies when the server has no target yet — a defined
/// server value always wins. Skipped while the user has the field focused
/// (mid-edit — they may pause for as long as they like): in that case the
/// server still holds the *previous* value and re-seeding would clobber the
/// in-progress input.
pub(crate) fn target_sync() {
    if dom::input("target").matches(":focus").unwrap_or(false) {
        return;
    }

    // `None` = the server has no target: keep whatever is in the field (the
    // default set on init, or a local edit) instead of clobbering it.
    let seed = state::with_state(|s| s.server.as_ref().and_then(|srv| srv.bms.target_soc));
    if let Some(seed) = seed {
        let pct = (seed * 100.0).round() as u32;
        dom::input("target").set_value(&pct.to_string());
        sync_target_ui();
    }
}

/// Default the deadline date/time inputs to "now + 4 h" once, after the first
/// state. The date and time are independent inputs: on mobile each opens its
/// own picker, so picking a new time keeps the date.
pub(crate) fn ensure_deadline_default() {
    if with_ui_ref(|ui| ui.deadline_set) || state::with_state(|s| s.server.is_none()) {
        return;
    }
    let date = dom::input("deadline-date");
    let time = dom::input("deadline-time");
    if !date.value().is_empty() || !time.value().is_empty() {
        return;
    }
    let ms = now_or_server_ts() + DEFAULT_DEADLINE_OFFSET_MS;
    date.set_value(&state::to_local_date(ms));
    time.set_value(&state::to_local_time(ms));
    with_ui(|ui| ui.deadline_set = true);
}

/// Re-seed the plan form from the active plan, so the user can start from
/// what is applied and tweak it: plan kind, deadline and power limit (the
/// target field is covered by `target_sync` — the plan's target SoC lives in
/// `bms.target_soc`). Like `target_sync`, fields being edited (focused)
/// are left alone. A missing plan does nothing: a stop must not clobber a
/// draft in progress.
/// The chip `data-preset` of a plan kind. Off-peak plans have no chip of
/// their own: they map to "unlimited" (no form options).
fn kind_preset(plan: &ChargingPlan) -> &'static str {
    match plan {
        ChargingPlan::Deadline { .. } => "deadline",
        ChargingPlan::Asap { .. } => "asap",
        ChargingPlan::Blocking => "blocking",
        _ => "unlimited",
    }
}

/// The plan kind of a chip `data-preset`.
fn preset_kind(preset: &str) -> Option<PlanKind> {
    match preset {
        "deadline" => Some(PlanKind::Deadline),
        "asap" => Some(PlanKind::Asap),
        "blocking" => Some(PlanKind::Blocking),
        "unlimited" => Some(PlanKind::Unlimited),
        _ => None,
    }
}

pub(crate) fn plan_form_sync() {
    let applied =
        state::with_state(|s| s.server.as_ref().and_then(|srv| srv.charging_plan.clone()));
    let Some(applied) = applied else {
        return;
    };
    let plan = &applied.plan;

    // Plan kind (chip + which option fields are visible).
    set_kind_ui(kind_preset(plan));

    match plan {
        ChargingPlan::Deadline {
            deadline,
            power_limit,
            ..
        } => {
            let date = dom::input("deadline-date");
            let time = dom::input("deadline-time");
            if !date.matches(":focus").unwrap_or(false) && !time.matches(":focus").unwrap_or(false)
            {
                // The plan carries a time of day only; mirror the server's
                // "today or tomorrow" rule — next occurrence of that time.
                let hhmm = format!("{:02}:{:02}", deadline.hour(), deadline.minute());
                let now_ms = now_or_server_ts();
                let mut ms = state::from_local_date_time(&state::to_local_date(now_ms), &hhmm)
                    .unwrap_or_else(|| now_ms + DEFAULT_DEADLINE_OFFSET_MS);
                if ms <= now_ms {
                    ms += 86_400_000;
                }
                let want_date = state::to_local_date(ms);
                let want_time = state::to_local_time(ms);
                if date.value() != want_date || time.value() != want_time {
                    date.set_value(&want_date);
                    time.set_value(&want_time);
                }
            }
            sync_power_limit(*power_limit);
        }
        ChargingPlan::Asap { power_limit, .. } => {
            sync_power_limit(*power_limit);
        }
        // Blocking / unlimited / off-peak plans have no form fields to mirror.
        _ => {}
    }
}

/// Mirror a plan's power limit into the chip row (and `UI.power_limit`).
fn sync_power_limit(power_limit: u32) {
    let changed = with_ui(|ui| {
        if ui.power_limit == power_limit {
            false
        } else {
            ui.power_limit = power_limit;
            true
        }
    });
    if changed {
        chip_sync("power-limit-chips", &power_limit.to_string());
    }
}

fn plan_set_msg(text: &str, ok: bool) {
    let el = dom::el("plan-msg");
    dom::set_text(&el, text);
    el.set_class_name(if text.is_empty() {
        "msg"
    } else if ok {
        "msg ok"
    } else {
        "msg err"
    });
}

fn plan_render_preview(p: Option<&PlanPreview>) {
    let el = dom::el("preview-panel");
    let Some(p) = p else {
        dom::add_class(&el, "hidden");
        return;
    };
    let mut rows: Vec<(&str, String)> = vec![
        (
            "Energy to add",
            format!("{:.1} kWh", p.energy as f64 / 1000.0),
        ),
        (
            "SoC",
            format!(
                "+{:.1} % → {:.1} %",
                p.soc_delta * 100.0,
                p.soc_result * 100.0
            ),
        ),
        ("Duration", state::fmt_dur(p.duration)),
    ];
    // Same "start → end" row as the status tab's plan card (hidden when the
    // preview has no schedule, e.g. already at target).
    if let Some(first) = p.periods.first() {
        rows.push(("Schedule", fmt_range(first.start_at.0, p.completes_at.0)));
    }
    rows.push((
        "Avg battery power",
        format!("{:.3} kW", p.avg_battery as f64 / 1000.0),
    ));
    let mut html = String::from("<h2>Preview</h2>");
    if !p.feasible {
        html.push_str(&format!(
            "<div class=\"warn\">{}</div>",
            p.note.as_deref().unwrap_or("Not feasible")
        ));
    }
    html.push_str("<div class=\"kv-grid\">");
    for (l, v) in &rows {
        html.push_str(&format!(
            "<div class=\"kv\"><span>{l}</span><b>{v}</b></div>"
        ));
    }
    html.push_str("</div>");
    let periods: Vec<String> = p
        .periods
        .iter()
        .filter(|x| x.power_limit > 0)
        .map(|x| {
            let end_ms = x.start_at.0 + x.duration as i64 * 1000;
            format!(
                "<div class=\"period\"><span>{s} – {e}</span><b>@ {lim:.3} kW limit ≈ {bat:.3} kW to battery</b></div>",
                s = state::fmt_time(x.start_at.0, true),
                e = state::fmt_time(end_ms, true),
                lim = x.power_limit as f64 / 1000.0,
                bat = (x.power_limit as f64 - bms::STATIC_LOSS as f64) / 1000.0,
            )
        })
        .collect();
    if !periods.is_empty() {
        html.push_str("<h3>Periods</h3>");
        html.push_str(&periods.join(""));
    }
    dom::remove_class(&el, "hidden");
    dom::set_inner_html(&el, &html);
}

/// Preview the current form **client-side** (shared `plan::compute`).
///
/// The debounce keeps this off the hot path. This is a synchronous local
/// computation, so it does not touch `plan_req_id` (bumping it here would
/// suppress the "Plan applied." message of an apply that is still in flight).
fn plan_do_preview() {
    let Some(body) = plan_build_body() else {
        plan_set_msg("Pick a deadline.", false);
        return;
    };
    // Option-less kinds (blocking / unlimited) have nothing to preview.
    if matches!(body.kind, PlanKind::Blocking | PlanKind::Unlimited) {
        plan_render_preview(None);
        plan_set_msg("", true);
        return;
    }
    let Some(p) = plan_local_preview(&body) else {
        plan_render_preview(None);
        plan_set_msg(
            "No SoC known yet: set the initial SoC or wait for the first sample.",
            false,
        );
        return;
    };
    plan_render_preview(Some(&p));
    plan_set_msg("", true);
}

pub(crate) fn plan_schedule_preview() {
    with_ui(|ui| {
        if let Some(t) = ui.plan_timer.take() {
            dom::window().clear_timeout_with_handle(t);
        }
    });
    // The pending `setTimeout` holds the callback until it fires (or is
    // cleared above), so it must not be piled into `UI.alive` (that would leak
    // one closure per call — `render()` calls this on every SSE event).
    let func = dom::closure(|_e| plan_do_preview());
    let t = dom::window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(&func, PREVIEW_DEBOUNCE_MS)
        .expect("setTimeout");
    with_ui(|ui| ui.plan_timer = Some(t));
}

// --- plan prep (shared math with the server, `charge_system_web_model::plan`)

/// The SoC the plan math starts from, using the shared re-estimation rule:
/// a known initial SoC plus the session energy delivered so far (same math the
/// server applies on `/api/bms/init_soc`, so the preview reacts to the widget
/// instantly and the state catch-up later confirms it). Falls back to the live
/// sample SoC when no initial SoC is known.
fn plan_start_soc() -> Option<f32> {
    state::with_state(|s| {
        let srv = s.server.as_ref()?;
        srv.bms
            .current_soc
            .absolute()
            .or(srv.bms.initial_soc.absolute())
            .or_else(|| with_ui_ref(|ui| ui.init_soc))
    })
}

/// The lowest target SoC (in integer %) a plan can be applied with: the
/// current SoC (re-estimated from the initial SoC, or the last sample), or
/// the initial-SoC widget value when no current SoC is known. A target below
/// the floor is meaningless (the battery is already there), so applying is
/// blocked; a target *at* the floor is allowed (it just does nothing).
fn target_floor() -> Option<u32> {
    let soc = plan_start_soc()?;
    Some((soc * 100.0).ceil() as u32)
}

/// The power-limit value in W, chosen by the preset chips in the form.
fn plan_power_limit() -> u32 {
    with_ui_ref(|ui| ui.power_limit)
}

/// Highlight the preset chip matching the current value, if any (chips are
/// mutually exclusive, so at most one is active).
fn chip_sync(container_id: &str, value: &str) {
    let chips = dom::query_all(&format!("#{container_id} .chip"));
    for i in 0..chips.length() {
        let Some(chip) = chips.item(i) else {
            continue;
        };
        let chip = chip
            .dyn_into::<web_sys::Element>()
            .expect("chip is an element");
        let active = chip
            .get_attribute("data-preset")
            .map(|p| p == value)
            .unwrap_or(false);
        if active {
            dom::add_class(&chip, "active");
        } else {
            dom::remove_class(&chip, "active");
        }
    }
}

/// Select the plan-kind chip and show/hide the option fields it allows:
/// deadline + power limit for Deadline, target + power limit for ASAP, and
/// none at all for Blocking / Unlimited.
fn set_kind_ui(kind: &str) {
    chip_sync("plan-kind-chips", kind);
    for (id, hide) in [
        ("target-wrap", !matches!(kind, "deadline" | "asap")),
        ("power-limit-wrap", !matches!(kind, "deadline" | "asap")),
        ("deadline-wrap", kind != "deadline"),
    ] {
        let el = dom::el(id);
        if hide {
            dom::add_class(&el, "hidden");
        } else {
            dom::remove_class(&el, "hidden");
        }
    }
}

/// The plan kind selected in the form (the active chip).
fn selected_kind() -> Option<PlanKind> {
    let chips = dom::query_all("#plan-kind-chips .chip");
    for i in 0..chips.length() {
        let Some(chip) = chips.item(i) else {
            continue;
        };
        let chip = chip
            .dyn_into::<web_sys::Element>()
            .expect("chip is an element");
        if !chip.class_list().contains("active") {
            continue;
        }
        return preset_kind(chip.get_attribute("data-preset").as_deref()?);
    }
    None
}

/// Build the request body from the form, or `None` when a deadline is needed
/// but missing. Blocking / Unlimited carry no options (`target_soc`,
/// `deadline` and `power_limit` are `None`); `target_soc` is a *fraction*
/// (0–1) on the wire.
fn plan_build_body() -> Option<PlanRequest> {
    let kind = selected_kind()?;
    match kind {
        // No options to convey.
        PlanKind::Blocking | PlanKind::Unlimited => Some(PlanRequest {
            kind,
            target_soc: None,
            deadline: None,
            initial_soc: with_ui_ref(|ui| ui.init_soc),
            power_limit: None,
        }),
        PlanKind::Deadline | PlanKind::Asap => {
            let deadline_date = dom::input("deadline-date");
            let deadline_time = dom::input("deadline-time");
            let target = dom::input("target");
            let target_soc = target.value().parse::<f64>().ok()? / 100.0;
            Some(PlanRequest {
                kind,
                target_soc: Some(target_soc as f32),
                deadline: match kind {
                    PlanKind::Deadline => {
                        // Missing date/time -> `None` body -> "Pick a deadline."
                        let ms = state::from_local_date_time(
                            &deadline_date.value(),
                            &deadline_time.value(),
                        )?;
                        Some(Ts(ms))
                    }
                    _ => None,
                },
                initial_soc: with_ui_ref(|ui| ui.init_soc),
                power_limit: Some(plan_power_limit()),
            })
        }
    }
}

/// Preview the plan **client-side** with the shared `plan::compute`: the
/// front end already holds the `Bms` (from `/api/state`) and the
/// current SoC, and the server recomputes the identical preview when the plan
/// is applied (single implementation, `charge_system_web_model::plan`).
fn plan_local_preview(body: &PlanRequest) -> Option<PlanPreview> {
    let cur_soc = plan_start_soc()?;
    // The option-less kinds have nothing to preview.
    let (target_soc, power_limit) = match body.kind {
        PlanKind::Deadline | PlanKind::Asap => (body.target_soc?, body.power_limit?),
        PlanKind::Blocking | PlanKind::Unlimited => return None,
    };

    state::with_state(|s| {
        s.server.as_ref().map(|srv| {
            let now = chrono::Utc::now();
            let deadline = body.deadline.map(|t| t.to_utc());
            plan_math::compute(
                body.kind,
                now,
                cur_soc,
                target_soc,
                deadline,
                power_limit,
                &srv.bms,
            )
        })
    })
}

/// Update the target-SoC label and chip highlight from the current field
/// value. (The deadline field's visibility is owned by `set_kind_ui`.)
fn sync_target_ui() {
    let target = dom::input("target");
    let pct = target
        .value()
        .parse::<f64>()
        .map(|v| v.round())
        .unwrap_or(0.0);
    dom::set_text(&dom::el("target-val"), &format!("{pct:.0} %"));
    chip_sync("target-chips", &pct.to_string());
}

fn init_soc_set_ui(pct: Option<u32>, note: &str) {
    let input = dom::input("init-soc");
    // `None` = nothing known: leave the field and chip highlight untouched
    // (writing a value like `0` would show a number the server never reported).
    if let Some(p) = pct {
        input.set_value(&p.to_string());
        chip_sync("init-soc-chips", &p.to_string());
    }
    dom::set_text(
        &dom::el("init-soc-val"),
        &match pct {
            Some(p) => format!("{p} %"),
            None => "… (unknown)".into(),
        },
    );
    dom::set_text(&dom::el("init-soc-note"), note);
}

/// Re-derive the init-SoC widget from the server state: the session's
/// `initial_soc` when a session is active, else the last value we pushed.
///
/// Skipped while the user has the field focused (mid-edit — they may pause
/// for as long as they like) or while a commit is in flight: in both cases
/// the server still holds the *previous* value and re-seeding would clobber
/// the in-progress input. The commit's echo (a `Bms` stream event) re-renders
/// once the server state catches up.
pub(crate) fn init_soc_sync() {
    if dom::input("init-soc").matches(":focus").unwrap_or(false)
        || with_ui_ref(|ui| ui.init_commits_in_flight) > 0
    {
        return;
    }

    match state::with_state(|s| s.server.as_ref().map(|srv| srv.bms.initial_soc.absolute())) {
        Some(server_seed) => {
            let seed = server_seed.or_else(|| with_ui_ref(|ui| ui.init_soc));
            with_ui(|ui| ui.init_soc = seed);
            let note = match seed {
                Some(_) => "Last known SoC at the start of the session.",
                None => "No SoC known yet. Set it so the plan math can start from a real value.",
            };
            init_soc_set_ui(seed.map(|s| (s * 100.0).round() as u32), note);
        }
        None => init_soc_set_ui(None, ""),
    }
}

// --- Remaining-time countdown -------------------------------------------

/// While the remaining time is below this, show seconds on a per-second
/// tick; above it, a per-minute update at minute granularity suffices.
const COUNTDOWN_FINE_BELOW_MS: i64 = 3 * 60_000;
const COUNTDOWN_FINE_PERIOD_MS: i32 = 1_000;
const COUNTDOWN_COARSE_PERIOD_MS: i32 = 60_000;

/// Interpolated remaining time (ms) from the last server re-baseline, or
/// `None` when no server value is known yet.
fn remaining_now(s: &ClientState, now: i64) -> Option<i64> {
    s.remaining_at
        .map(|(secs, at)| (secs * 1_000 - (now - at)).max(0))
}

/// Display format for the remaining time: `MM:SS` below the fine threshold,
/// `H h MM min` / `M min` above it.
pub(crate) fn fmt_remaining(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < COUNTDOWN_FINE_BELOW_MS {
        let total_s = ms / 1_000;
        format!("{:02}:{:02}", total_s / 60, total_s % 60)
    } else {
        state::fmt_dur((ms / 1_000) as u64)
    }
}

/// Update the plan card's "Remaining time" cell (which only exists once a
/// plan has been rendered).
fn update_remaining_texts() {
    let Some(ms) = state::with_state(|s| remaining_now(s, now_ms())) else {
        return;
    };
    if let Some(el) = dom::el_option("plan-remaining") {
        dom::set_text(&el, &fmt_remaining(ms));
    }
}

/// Re-baseline the countdown from the fresh server state (the live charge
/// progress's remaining time, the same source the `status` SSE events use).
/// Called after each state fetch.
pub(crate) fn countdown_rebaseline_from_state() {
    let secs = state::with_state(|s| {
        s.server.as_ref().and_then(|srv| match &srv.status {
            ChargeStatus::Charging(p) | ChargeStatus::SuspendedEvse(p) => {
                Some(p.remaining_time as i64)
            }
            _ => None,
        })
    });
    if let Some(secs) = secs {
        state::with_state(|s| s.remaining_at = Some((secs, now_ms())));
    }
}

/// The adaptive countdown timer's tick: updates the remaining-time texts
/// only. When the countdown reaches zero, drop the re-baseline and stop the
/// timer (the next server update re-baselines and re-arms it).
pub(crate) fn countdown_tick() {
    let done = state::with_state(|s| {
        if let Some((secs, at)) = s.remaining_at
            && secs * 1_000 - (now_ms() - at) <= 0
        {
            s.remaining_at = None;
            return true;
        }
        false
    });
    update_remaining_texts();
    if done {
        set_countdown_timer(None);
    }
}

/// Decide and apply the countdown timer: per-minute tick while ≥ 3 min
/// remain, per-second below; no timer otherwise (no plan, or no re-baseline).
/// Also refreshes the texts immediately, so the UI never waits a whole period
/// for the first update.
pub(crate) fn countdown_sync() {
    let period = state::with_state(|s| {
        let srv = s.server.as_ref()?;
        // A plan with a schedule counts down even before it starts.
        let applied = srv.charging_plan.as_ref()?;
        if applied.periods.is_empty() {
            // No schedule (e.g. blocking): nothing to count down — drop any
            // stale re-baseline so the previous plan's time is not shown.
            s.remaining_at = None;
            return None;
        }
        let start_ms = match applied.periods.first().map(|p| p.start_at.0) {
            Some(ms) => ms,
            None => return None,
        };
        let end = applied.end()?;
        if session_active(&srv.status) {
            // A live session re-baselines from charge progress (`on_status`
            // and state fetches); only fill in when none is set yet (e.g.
            // just switched to Preparing).
            if s.remaining_at.is_none() {
                let secs = ((end.0 - now_ms()) / 1000).max(0);
                if secs > 0 {
                    s.remaining_at = Some((secs, now_ms()));
                }
            }
        } else {
            // No live progress: show the *charging time* still to come — the
            // whole window before it starts, then its remaining part; nothing
            // once the schedule has ended.
            let now = now_ms();
            let secs = if now < start_ms {
                (end.0 - start_ms) / 1000
            } else if now < end.0 {
                (end.0 - now) / 1000
            } else {
                0
            };
            s.remaining_at = (secs > 0).then_some((secs, now));
        }
        match remaining_now(s, now_ms()) {
            Some(ms) if ms > 0 => Some(if ms < COUNTDOWN_FINE_BELOW_MS {
                COUNTDOWN_FINE_PERIOD_MS
            } else {
                COUNTDOWN_COARSE_PERIOD_MS
            }),
            _ => None,
        }
    });
    set_countdown_timer(period);
    update_remaining_texts();
}

/// (Re)start the countdown timer when its period changes, clear it when no
/// longer needed. This is the app's only timer; there are no permanent ones.
fn set_countdown_timer(period_ms: Option<i32>) {
    let changed = with_ui(|ui| {
        if ui.countdown_period == period_ms {
            return false;
        }
        if let Some(handle) = ui.countdown_timer.take() {
            dom::window().clear_interval_with_handle(handle);
        }
        ui.countdown_period = period_ms;
        true
    });
    if !changed {
        return;
    }
    if let Some(period) = period_ms {
        // The running interval holds the callback until cleared, so it must
        // not be piled into `UI.alive` (it is re-created on every period
        // change).
        let func = dom::closure(|_e| countdown_tick());
        let handle = dom::window()
            .set_interval_with_callback_and_timeout_and_arguments_0(&func, period)
            .expect("setInterval");
        with_ui(|ui| ui.countdown_timer = Some(handle));
    }
}

/// Parse a number input's value (integer %), update its label + chip
/// highlight, and return the value. Shared by the target-SoC and init-SoC
/// widgets.
fn local_sync_num(input: &HtmlInputElement, val_id: &str, chips_id: &str) -> Option<u32> {
    let pct = input.value().parse::<u32>().ok()?;
    dom::set_text(&dom::el(val_id), &format!("{pct} %"));
    chip_sync(chips_id, &pct.to_string());
    Some(pct)
}

/// Wire every `.chip` in `container_id` to `on_click(preset)`.
fn wire_chips(container_id: &str, on_click: Rc<dyn Fn(String)>) {
    let chips = dom::query_all(&format!("#{container_id} .chip"));
    for i in 0..chips.length() {
        let Some(chip) = chips.item(i) else {
            continue;
        };
        let chip: web_sys::HtmlButtonElement =
            chip.dyn_into::<web_sys::HtmlButtonElement>().expect("chip");
        let preset = chip
            .get_attribute("data-preset")
            .expect("chip has data-preset");
        let oc = on_click.clone();
        on(&chip, "click", move |_e| oc(preset.clone()));
    }
}

pub(crate) fn init_plan(on_change: Rc<dyn Fn() + 'static>) {
    let target = dom::input("target");
    let deadline_date = dom::input("deadline-date");
    let deadline_time = dom::input("deadline-time");
    let init_soc = dom::input("init-soc");

    // Target-SoC: per-keystroke updates stay local (the preview is a
    // `PREVIEW_DEBOUNCE_MS`-debounced, client-side computation). The value is
    // conveyed to the server when the plan is applied (`PlanRequest.target_soc`);
    // `target_sync` re-seeds the field from `bms.target_soc` once the echo lands.
    let target_local_sync = move || -> bool {
        local_sync_num(&dom::input("target"), "target-val", "target-chips").is_some()
    };

    on(&target, "input", move |_e| {
        if (target_local_sync)() {
            plan_schedule_preview();
        }
    });
    on(&target, "change", move |_e| {
        // Done editing (Enter, or blur with a changed value): keep the new
        // value — or restore the last valid one if the field doesn't parse.
        if !(target_local_sync)() {
            let pct = state::with_state(|s| {
                s.server
                    .as_ref()
                    .and_then(|srv| srv.bms.target_soc)
                    .map(|f| (f * 100.0).round() as u32)
            })
            .unwrap_or(DEFAULT_TARGET_PCT);
            dom::input("target").set_value(&pct.to_string());
            (target_local_sync)();
        }
    });
    // Init-SoC: per-keystroke updates stay local (the preview is debounced,
    // client-side); the value is pushed to the server once, when editing
    // finishes (`change` = blur or Enter) or a preset chip is tapped. An
    // invalid/empty field keeps the last valid value and never commits — so
    // clearing then re-typing on a phone keyboard sends exactly one request
    // with the final value.
    let init_soc_local_sync = move || -> bool {
        let Some(pct) = local_sync_num(&dom::input("init-soc"), "init-soc-val", "init-soc-chips")
        else {
            return false;
        };
        with_ui(|ui| ui.init_soc = Some(pct as f32 / 100.0));
        true
    };

    let init_soc_commit = move || {
        let Some(pct) = dom::input("init-soc").value().parse::<u32>().ok() else {
            return;
        };
        let soc = pct as f32 / 100.0;
        let id = with_ui(|ui| {
            ui.plan_req_id += 1;
            ui.init_commits_in_flight += 1;
            ui.plan_req_id
        });
        spawn_local(async move {
            if let Err(e) = api::set_init_soc(soc).await {
                console_error("set init SoC", &e);
            }
            let latest = with_ui(|ui| {
                ui.init_commits_in_flight -= 1;
                ui.plan_req_id == id
            });
            if latest {
                plan_schedule_preview();
            }
        });
    };

    // Live local preview while typing; the server is only told once editing
    // finishes. While the field is focused / a commit is in flight, the render
    // must not re-seed it from stale server state — see `init_soc_sync`.
    on(&init_soc, "input", move |_e| {
        if (init_soc_local_sync)() {
            plan_schedule_preview();
        }
    });
    on(&init_soc, "change", move |_e| {
        // Done editing (Enter, or blur with a changed value): commit the new
        // value — or restore the last valid one if the field doesn't parse
        // (nothing to send in that case).
        if (init_soc_local_sync)() {
            (init_soc_commit)();
        } else if let Some(pct) = with_ui_ref(|ui| ui.init_soc).map(|s| (s * 100.0).round() as u32)
        {
            dom::input("init-soc").set_value(&pct.to_string());
            (init_soc_local_sync)();
        }
    });
    // Plan-kind chips: picking one shows the option fields it allows.
    wire_chips(
        "plan-kind-chips",
        Rc::new(|preset| {
            set_kind_ui(&preset);
            plan_schedule_preview();
        }),
    );
    // Date and time are independent: changing either re-previews with the
    // other input's current value (the preview is a local computation — no
    // server fetch needed until the plan is applied).
    on(&deadline_date, "input", {
        move |_e| {
            plan_schedule_preview();
        }
    });
    on(&deadline_time, "input", {
        move |_e| {
            plan_schedule_preview();
        }
    });
    // Target preset chips: set the matching input and update locally.
    {
        let target = target.clone();
        wire_chips(
            "target-chips",
            Rc::new(move |preset| {
                target.set_value(&preset);
                if local_sync_num(&dom::input("target"), "target-val", "target-chips").is_some() {
                    plan_schedule_preview();
                }
            }),
        );
    }
    // Init-SoC preset chips: a chip tap is "done editing" — update locally
    // and commit at once (no blur follows to trigger `change`).
    wire_chips(
        "init-soc-chips",
        Rc::new(move |preset| {
            dom::input("init-soc").set_value(&preset);
            if (init_soc_local_sync)() {
                (init_soc_commit)();
            }
        }),
    );
    // Power-limit chips: pick the preset directly (no input to mirror).
    wire_chips(
        "power-limit-chips",
        Rc::new(|preset| {
            let Some(limit) = preset.parse::<u32>().ok() else {
                return;
            };
            with_ui(|ui| ui.power_limit = limit);
            chip_sync("power-limit-chips", &preset);
            plan_schedule_preview();
        }),
    );

    // Apply (direct, or through the confirm dialog when a real plan is in
    // effect).
    on(&dom::el_html("plan-apply"), "click", {
        let oc = on_change.clone();
        move |_e| {
            let Some(body) = plan_build_body() else {
                plan_set_msg("Pick a deadline.", false);
                return;
            };
            if matches!(body.kind, PlanKind::Deadline | PlanKind::Asap) {
                if plan_start_soc().is_none() {
                    plan_set_msg(
                        "No SoC known yet: set the initial SoC or wait for the first sample.",
                        false,
                    );
                    return;
                }
                let target_pct = body.target_soc.expect("set above") as f64 * 100.0;
                if let Some(floor) = target_floor()
                    && target_pct < floor as f64
                {
                    plan_set_msg(
                        &format!("Target SoC must be ≥ current SoC ({floor} %)."),
                        false,
                    );
                    return;
                }
            }
            // Replacing a plan that actually charges (deadline / ASAP) needs
            // an explicit confirm.
            let active = state::with_state(|s| {
                s.server
                    .as_ref()
                    .and_then(|srv| srv.charging_plan.as_ref())
                    .map(|applied| applied.plan)
            });
            match active {
                Some(p @ (ChargingPlan::Deadline { .. } | ChargingPlan::Asap { .. })) => {
                    let body = body.clone();
                    let oc = oc.clone();
                    confirm_show(
                        format!("Replace the active {} plan?", plan_name(&p)),
                        Box::new(move || do_apply_plan(body, &oc)),
                    );
                }
                _ => do_apply_plan(body, &oc),
            }
        }
    });

    let limit = plan_power_limit();
    chip_sync("power-limit-chips", &limit.to_string());

    init_soc_sync();
}

/// Send the plan to the server (the apply button path — direct or after the
/// confirm dialog). The request id guards against out-of-order responses.
fn do_apply_plan(body: PlanRequest, on_change: &Rc<dyn Fn() + 'static>) {
    let apply_btn: HtmlButtonElement = dom::el_html("plan-apply")
        .dyn_into()
        .expect("apply is a button");
    apply_btn.set_disabled(true);
    let id = with_ui(|ui| {
        ui.plan_req_id += 1;
        ui.apply_in_flight += 1;
        ui.plan_req_id
    });
    let oc = on_change.clone();
    spawn_local(async move {
        let res = api::apply_plan(&body).await;
        // The button re-enables only when the *last* in-flight apply settles:
        // `plan_req_id` alone can't gate this — an init-SoC commit may bump it
        // mid-flight, which would leave the button disabled forever.
        let (latest, in_flight) = with_ui(|ui| {
            ui.apply_in_flight -= 1;
            (ui.plan_req_id == id, ui.apply_in_flight)
        });
        if latest {
            match res {
                Ok(_) => plan_set_msg("Plan applied.", true),
                Err(e) => plan_set_msg(&e, false),
            }
        }
        if in_flight == 0 {
            apply_btn.set_disabled(false);
        }
        (oc)();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_remaining_switches_to_seconds_below_three_minutes() {
        assert_eq!(fmt_remaining(3 * 60_000), "3 min");
        assert_eq!(fmt_remaining(3 * 60_000 - 1_000), "02:59");
        assert_eq!(fmt_remaining(15_120_000), "4 h 12 min");
        assert_eq!(fmt_remaining(0), "00:00");
        assert_eq!(fmt_remaining(-5_000), "00:00");
    }

    #[test]
    fn remaining_now_interpolates_down_and_clamps() {
        let mut s = ClientState {
            remaining_at: Some((120, 0)), // 120 s at client instant 0
            ..Default::default()
        };
        assert_eq!(remaining_now(&s, 0), Some(120_000));
        assert_eq!(remaining_now(&s, 30_000), Some(90_000));
        assert_eq!(remaining_now(&s, 120_000), Some(0));
        assert_eq!(remaining_now(&s, 200_000), Some(0)); // clamped at zero
        s.remaining_at = None;
        assert_eq!(remaining_now(&s, 0), None);
    }
}
