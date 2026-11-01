//! Tab bar: the tab buttons and the metric-controls (window scale) buttons.

use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlButtonElement;

use super::catchup::{RefreshScope, refresh};
use crate::ui::on;
use crate::{dom, plot, state};

/// Activate tab `name` — button click, swipe or wheel: toggle the DOM,
/// (re)draw the charts when entering metrics, and persist the selection.
pub(crate) fn activate_tab(name: &str) {
    let all_btns = dom::query_all(".tab");
    for i in 0..all_btns.length() {
        if let Some(x) = all_btns.item(i) {
            let x = x.unchecked_into::<web_sys::Element>();
            if dom::attr(&x, "data-tab").as_deref() == Some(name) {
                dom::add_class(&x, "active");
            } else {
                dom::remove_class(&x, "active");
            }
        }
    }
    let pane_id = format!("tab-{name}");
    let all_panes = dom::query_all(".tabpane");
    for i in 0..all_panes.length() {
        if let Some(p) = all_panes.item(i) {
            let p = p.unchecked_into::<web_sys::Element>();
            if dom::id_of(&p) == pane_id {
                dom::add_class(&p, "active");
            } else {
                dom::remove_class(&p, "active");
            }
        }
    }
    if name == "metrics" {
        plot::sync();
    } else {
        // The Metrics pane is leaving: a tooltip pinned by a touch tap must
        // not linger on the hidden charts.
        plot::hide_all_tips();
    }
    state::with_state(|s| s.tab = name.to_string());
    state::save_prefs();
}

pub(crate) fn init_tabs() {
    let btns = dom::query_all(".tab");
    let n = btns.length();
    for i in 0..n {
        let Some(b) = btns.item(i) else {
            continue;
        };
        let tab = dom::attr(&b, "data-tab").unwrap_or_default();
        on(&b, "click", move |_e| {
            activate_tab(&tab);
        });
    }
}

pub(crate) fn init_metrics_controls() {
    // Metric toggles.
    let tseg = dom::el_html("metric-toggles");
    let mut html = String::new();
    for m in plot::METRICS {
        let active = state::is_enabled(m.key);
        html.push_str(&format!(
            "<button class=\"btn{}\" data-m=\"{}\">{}</button>",
            if active { " active" } else { "" },
            m.key,
            m.label,
        ));
    }
    dom::set_inner_html(&tseg, &html);
    let buttons = tseg.query_selector_all("button").expect("toggle buttons");
    for i in 0..buttons.length() {
        let Some(b) = buttons.item(i) else {
            continue;
        };
        let b = b.unchecked_into::<HtmlButtonElement>();
        let key = dom::attr(&b, "data-m").unwrap_or_default();
        let b2 = b.clone();
        on(&b, "click", move |_e| {
            state::mark_user_toggled(&key);
            let now_on = state::toggle(&key);
            if now_on {
                dom::add_class(&b2, "active");
            } else {
                dom::remove_class(&b2, "active");
            }
            plot::sync();
            state::save_prefs();
        });
    }

    // Window scale buttons.
    let scales: [(&str, i64); 5] = [
        ("10 min", 600_000),
        ("1 h", 3_600_000),
        ("6 h", 21_600_000),
        ("24 h", 86_400_000),
        ("7 d", 604_800_000),
    ];
    let seg = dom::el_html("scale-btns");
    let cur = state::with_state(|s| s.window);
    let mut html = String::new();
    for (l, ms) in &scales {
        html.push_str(&format!(
            "<button class=\"btn{}\" data-ms=\"{}\">{l}</button>",
            if *ms == cur { " active" } else { "" },
            ms,
        ));
    }
    dom::set_inner_html(&seg, &html);
    let buttons = seg.query_selector_all("button").expect("scale buttons");
    let scale_btns: Vec<HtmlButtonElement> = (0..buttons.length())
        .filter_map(|i| buttons.item(i))
        .map(|b| b.unchecked_into())
        .collect();
    // Shared handle cloned once per button (not a whole-vec clone per button).
    let all = Rc::new(scale_btns);
    for b in all.iter() {
        let ms = dom::attr(b, "data-ms")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(state::DEFAULT_WINDOW_MS);
        let all = all.clone();
        let b2 = b.clone();
        on(b, "click", move |_e| {
            state::with_state(|s| s.window = ms);
            let b2_node = b2.unchecked_ref::<web_sys::Node>();
            for x in all.iter() {
                let x_node = x.unchecked_ref::<web_sys::Node>();
                if x_node.is_same_node(Some(b2_node)) {
                    dom::add_class(x, "active");
                } else {
                    dom::remove_class(x, "active");
                }
            }
            state::save_prefs();
            // Only the sample window changed: fetch it alone.
            refresh(RefreshScope::SamplesOnly);
        });
    }
}
