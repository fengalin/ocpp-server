//! Hand-rolled canvas line plotter (replaces Chart.js).
//!
//! One chart per enabled metric, ported 1:1 from the old `charts.ts`:
//! linear epoch-ms x axis, bucketed downsample to <= 700 points,
//! spanGaps (a missing value breaks the line), fixed colors, `beginAtZero`
//! (+ suggested max ~8 kW) for power. Redraws are event-driven (SSE sample,
//! tab switch, window resize — there is no periodic tick); canvas backing
//! stores are scaled by `devicePixelRatio` for crisp lines.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlElement, MouseEvent, PointerEvent};

use charge_system_common_model::meter_value::MeterValueSelection;

use super::dom;
use super::state;

/// Downsample cap, same as the old Chart.js config.
const MAX_POINTS: usize = 700;
/// Number of x ticks (Chart.js `maxTicksLimit: 6`).
const TICKS: i32 = 6;
/// Windows wider than this show weekday info on the x axis (the short
/// weekday at 0:00) instead of `HH:MM` everywhere.
const DAY_MS: i64 = 86_400_000;
/// Pointer within this many css px of a data point snaps to it (scrub).
const SNAP_PX: f64 = 18.0;
/// Horizontal touch travel (css px) beyond which the chart lets the gesture
/// go (it is the tab-switch swipe, handled in `ui`).
const TOUCH_SWIPE_PX: f64 = 16.0;
/// X-axis slack added at both ends of the window (ms).
const X_SLACK_MS: i64 = 60_000;
/// A time jump larger than this between points breaks the line (spanGaps).
/// Note: inferred from a gap because `None` values are pre-filtered — if the
/// meter interval ever exceeds this, the line would break spuriously.
const SPAN_GAP_MS: i64 = 3 * 60_000;
/// Fractional y headroom added above (and below, unless `begin_at_zero`) the
/// data range.
const Y_HEADROOM: f64 = 0.08;

/// Canvas insets (css px): room for y tick labels / x tick labels.
const PAD_L: f64 = 34.0;
const PAD_R: f64 = 10.0;
const PAD_T: f64 = 10.0;
const PAD_B: f64 = 22.0;
/// Minimum canvas size (css px) kept when the box collapses.
const MIN_CSS_W: f64 = 10.0;
const MIN_CSS_H: f64 = 10.0;
/// Minimum box size (css px) before the canvas is resized at all.
const MIN_BOX_PX: f64 = 16.0;
/// Box padding (css px) subtracted from the box size to get the canvas size.
const BOX_PAD_PX: f64 = 16.0;

/// Metric definition. `value_of` returns the *display* value (kW / kWh / V /
/// °C) or `None` when the sample carries no such field. `integer` marks
/// metrics whose values are whole numbers in practice (voltage, temperature):
/// their y ticks and tooltips show no decimal places.
#[derive(Clone, Copy)]
pub struct MetricDef {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub color: &'static str,
    pub begin_at_zero: bool,
    pub suggested_max: Option<f64>,
    pub integer: bool,
}

impl MetricDef {
    /// The display value of metric `key` in a sample, if present. Delegates
    /// to the single implementation in `state::metric_value_of`.
    pub fn value_of(key: &str, m: &MeterValueSelection) -> Option<f64> {
        state::metric_value_of(key, m)
    }

    /// Tooltip value format: whole numbers for `integer` metrics, 3 decimals
    /// otherwise.
    pub fn fmt_value(&self, v: f64) -> String {
        if self.integer {
            format!("{v:.0}")
        } else {
            format!("{v:.3}")
        }
    }
}

pub const METRICS: &[MetricDef] = &[
    MetricDef {
        key: "power",
        label: "Power",
        unit: "kW",
        color: "#4cc38a",
        begin_at_zero: true,
        suggested_max: Some(8.0),
        integer: false,
    },
    MetricDef {
        key: "temp",
        label: "Temperature",
        unit: "\u{00b0}C",
        color: "#e0705a",
        begin_at_zero: false,
        suggested_max: None,
        integer: true,
    },
    MetricDef {
        key: "voltage",
        label: "Voltage",
        unit: "V",
        color: "#e6b45a",
        begin_at_zero: false,
        suggested_max: None,
        integer: true,
    },
    MetricDef {
        key: "energy",
        label: "Energy",
        unit: "kWh",
        color: "#5aa9e6",
        begin_at_zero: false,
        suggested_max: None,
        integer: false,
    },
];

// The charts, by `MetricDef::key`. WASM is single-threaded, so a
// `thread_local` needs no `unsafe Sync` wrapper; access it only through the
// scoped `with_charts` / `with_charts_ref` helpers (a nested `borrow_mut`
// would panic and wedge the `RefCell`).
thread_local! {
    static CHARTS: RefCell<HashMap<String, Chart>> = RefCell::new(HashMap::new());
    /// The last downsampled series + window computed by `sync`/`redraw_all`,
    /// reused by the pointer scrub (which runs per `pointermove` and must not
    /// re-clone + re-downsample the samples every time).
    static LAST_DS: RefCell<Option<(Vec<MeterValueSelection>, i64)>> = const { RefCell::new(None) };
}

fn with_charts<R>(f: impl FnOnce(&mut HashMap<String, Chart>) -> R) -> R {
    CHARTS.with(|c| f(&mut c.borrow_mut()))
}

fn with_charts_ref<R>(f: impl FnOnce(&HashMap<String, Chart>) -> R) -> R {
    CHARTS.with(|c| f(&c.borrow()))
}

/// Publish the downsampled series the charts were last drawn from.
fn set_last_ds(ds: Vec<MeterValueSelection>, window: i64) {
    LAST_DS.with(|c| *c.borrow_mut() = Some((ds, window)));
}

struct Chart {
    canvas: HtmlCanvasElement,
    dpr: f64,
    css_w: f64,
    css_h: f64,
    /// X scale of the last draw (plot-area left/right edges in css px).
    xmin: i64,
    xmax: i64,
    plot_l: f64,
    plot_r: f64,
    /// Y scale of the last draw, in css px (for the tooltip's y position).
    plot_t: f64,
    plot_b: f64,
    ymin: f64,
    ymax: f64,
    /// Scrub popup: shown while a pointer is on the chart (hover or press).
    tip: HtmlElement,
    tip_ts: i64,
    tip_val: f64,
    /// X (css px, canvas-relative) of the point the tooltip is pinned to
    /// (`-1` when the tooltip is not up). Used to tell a tap on the pinned
    /// point (toggle off) from a tap near another point (toggle to it).
    tip_px: f64,
    /// Pointer id of the touch currently held on this chart (`0` = none).
    touch_id: i32,
    /// clientX (css px, canvas-relative) of the touch `pointerdown`.
    touch_x: f64,
}

/// The pointer event kinds the chart canvas listens to (replaces the old
/// magic `0..=4u8` dispatch).
#[derive(Clone, Copy)]
enum PointerKind {
    Down,
    Move,
    Up,
    Cancel,
    Leave,
}

impl Chart {
    /// Create the `chart-wrap`/`chart-box`/canvas DOM (same shape the old TS
    /// built) and size the backing store to the box.
    fn new(key: &str, label: &str) -> Self {
        let host = dom::el_html("charts");
        let wrap = dom::create("div");
        dom::add_class(&wrap, "chart-wrap");
        wrap.set_id(&format!("chart-{key}"));
        let h = dom::create("h3");
        dom::set_text(&h, label);
        let chart_box = dom::create("div");
        dom::add_class(&chart_box, "chart-box");
        let canvas: HtmlCanvasElement = dom::create("canvas").dyn_into().expect("canvas");
        // The pointer-scrub handlers derive the metric key from the canvas id
        // (`chart-<key>`), so the canvas itself must carry it (not just the
        // wrapper).
        canvas.set_id(&format!("chart-{key}"));
        let tip: HtmlElement = dom::create("div").dyn_into().expect("tip div");
        dom::add_class(&tip, "chart-tip");
        dom::add_class(&tip, "hidden");
        chart_box.append_child(&canvas).expect("append canvas");
        chart_box.append_child(&tip).expect("append tip");
        wrap.append_child(&h).expect("append h3");
        wrap.append_child(&chart_box).expect("append box");
        host.append_child(&wrap).expect("append wrap");

        let dpr = dom::window().device_pixel_ratio().max(1.0);
        let mut chart = Chart {
            canvas: canvas.clone(),
            dpr,
            css_w: 0.0,
            css_h: 0.0,
            xmin: 0,
            xmax: 0,
            plot_l: 0.0,
            plot_r: 0.0,
            plot_t: 0.0,
            plot_b: 0.0,
            ymin: 0.0,
            ymax: 0.0,
            tip,
            tip_ts: 0,
            tip_val: 0.0,
            tip_px: -1.0,
            touch_id: 0,
            touch_x: 0.0,
        };
        chart.resize();
        chart.attach_pointer_events();
        chart
    }

    fn resize(&mut self) {
        // The canvas fills `.chart-box` (display block, 8px padding each side).
        let chart_box = match self.canvas.parent_element() {
            Some(b) => b,
            None => return,
        };
        let r = chart_box.get_bounding_client_rect();
        if r.width() > 0.0 {
            self.css_w = (r.width().max(MIN_BOX_PX) - BOX_PAD_PX).max(MIN_CSS_W);
            self.css_h = (r.height().max(MIN_BOX_PX) - BOX_PAD_PX).max(MIN_CSS_H);
            self.size_canvas();
        }
    }

    fn size_canvas(&self) {
        let c = &self.canvas;
        c.set_width((self.css_w * self.dpr).round() as u32);
        c.set_height((self.css_h * self.dpr).round() as u32);
        dom::set_style(c, "width", &format!("{:.0}px", self.css_w));
        dom::set_style(c, "height", &format!("{:.0}px", self.css_h));
    }

    fn ctx(&self) -> Option<CanvasRenderingContext2d> {
        // A missing 2D context is `None` — do not cast an empty JS value into
        // a context (every draw call would then throw in JS).
        self.canvas
            .get_context("2d")
            .ok()?
            .and_then(|o| o.dyn_into::<CanvasRenderingContext2d>().ok())
    }

    /// Pointer/touch scrubbing: hover (mouse) or press/slide (touch) on the
    /// chart to snap to the nearest data point and read its value + timestamp
    /// in the `.chart-tip` popup. A touch tap *toggles* the popup: it shows
    /// while touching and stays pinned after the finger lifts, so the user
    /// can read it; touching the chart again closes it. Vertical page
    /// scrolling stays working (`touch-action: pan-y`).
    fn attach_pointer_events(&mut self) {
        // `setPointerCapture`/`releasePointerCapture` are plain methods on
        // `EventTarget` (the capture target is `this`), so call them directly.
        // Fetching them off the *event* first is what a DOM spec trap is: the
        // spec says calling `setPointerCapture` on a different element while
        // the event is dispatching must throw, which made the old
        // `Reflect.apply` path abort the whole wasm instance.
        for (ev, kind) in [
            ("pointerdown", PointerKind::Down),
            ("pointermove", PointerKind::Move),
            ("pointerup", PointerKind::Up),
            ("pointercancel", PointerKind::Cancel),
            ("pointerleave", PointerKind::Leave),
        ] {
            let c = self.canvas.clone();
            let c2 = c.clone();
            let c3 = c.clone();
            dom::add_listener(&c, ev, move |e: &web_sys::Event| {
                let p = e.unchecked_ref::<PointerEvent>();
                let key = c2
                    .id()
                    .strip_prefix("chart-")
                    .unwrap_or_default()
                    .to_string();
                let is_touch = p.pointer_type() == "touch";
                match kind {
                    PointerKind::Down => {
                        // Touch toggle: with the tooltip up, a tap on the
                        // pinned point closes it, while a tap elsewhere scrubs
                        // to the new point (below); with it hidden, the touch
                        // is recorded for scrubbing.
                        let mut scrub = true;
                        if is_touch {
                            let me = e.unchecked_ref::<web_sys::MouseEvent>();
                            let x = me.client_x() as f64 - c2.get_bounding_client_rect().left();
                            with_charts(|charts| {
                                if let Some(chart) = charts.get_mut(&key) {
                                    if chart.touch_id == 0 {
                                        chart.touch_id = p.pointer_id();
                                        // Remember the touch-down position so
                                        // `scrub` can tell a scrub from a
                                        // horizontal tab-swipe.
                                        chart.touch_x = x;
                                    }
                                    if !chart.tip.class_list().contains("hidden")
                                        && (chart.tip_px - x).abs() <= TOUCH_SWIPE_PX
                                    {
                                        // Tap on the pinned point: close it.
                                        scrub = false;
                                    }
                                }
                            });
                        }
                        if scrub {
                            let _ = c3.set_pointer_capture(p.pointer_id());
                            Chart::scrub(&c2, e);
                        }
                    }
                    PointerKind::Move => Chart::scrub(&c2, e),
                    PointerKind::Up => {
                        let claimed = with_charts(|charts| {
                            if let Some(chart) = charts.get_mut(&key)
                                && chart.touch_id == p.pointer_id()
                            {
                                chart.touch_id = 0;
                                true
                            } else {
                                false
                            }
                        });
                        if is_touch {
                            // Touch: the tooltip stays pinned where it was
                            // shown (or hidden, if the swipe was let go).
                            if claimed {
                                let _ = c3.release_pointer_capture(p.pointer_id());
                            }
                        } else {
                            // Mouse: click/press ends the tooltip (unchanged).
                            let _ = c3.release_pointer_capture(p.pointer_id());
                            Chart::hide_tip(&key);
                        }
                    }
                    PointerKind::Leave => {
                        // The mouse left the canvas (e.g. a press-drag that
                        // ended off-chart): hide the tooltip as a mouse
                        // release would.
                        if !is_touch {
                            Chart::hide_tip(&key);
                        }
                    }
                    PointerKind::Cancel => {
                        // `pointercancel` (the browser took the gesture, e.g.
                        // a scroll): drop the touch and the tooltip.
                        if is_touch {
                            with_charts(|charts| {
                                if let Some(chart) = charts.get_mut(&key)
                                    && chart.touch_id == p.pointer_id()
                                {
                                    chart.touch_id = 0;
                                }
                            });
                        }
                        Chart::hide_tip(&key);
                    }
                }
            });
        }
    }

    /// Pointer/touch scrubbing: hover (mouse) or press/slide (touch) on the
    /// chart to snap to the nearest data point and read its value + timestamp
    /// in the `.chart-tip` popup. A horizontal touch swipe is left alone (it
    /// is the tab-switch gesture, handled in `ui`); vertical page scrolling
    /// stays working via `touch-action: pan-y` on the page. The touch tap
    /// toggle (pin on release, close on re-touch) lives in the `pointerdown`/
    /// `pointerup` handlers above.
    fn scrub(canvas: &HtmlCanvasElement, e: &web_sys::Event) {
        let p = e.unchecked_ref::<PointerEvent>();
        let ptype = p.pointer_type();
        let m = e.unchecked_ref::<MouseEvent>();
        let key = canvas
            .id()
            .strip_prefix("chart-")
            .unwrap_or_default()
            .to_string();
        let rect = canvas.get_bounding_client_rect();
        // `MouseEvent` exposes `client_x`/`client_y` directly (same event).
        let x = m.client_x() as f64 - rect.left();
        let y = m.client_y() as f64 - rect.top();
        let scales = with_charts(|charts| {
            let chart = charts.get_mut(&key)?;
            // A horizontal touch swipe is the tab-switch gesture
            // (ui::init_swipe); let it go instead of scrubbing. `touch_x` is
            // the touch-down position (recorded on `pointerdown`).
            if ptype != "mouse" && chart.touch_id == p.pointer_id() {
                let adx = (x - chart.touch_x).abs();
                if adx > TOUCH_SWIPE_PX {
                    return None;
                }
            }
            Some((
                chart.xmin,
                chart.xmax,
                chart.plot_l,
                chart.plot_r,
                chart.plot_t,
                chart.plot_b,
                chart.ymin,
                chart.ymax,
                chart.css_w,
                chart.css_h,
            ))
        });
        let Some((xmin, xmax, plot_l, plot_r, plot_t, plot_b, ymin, ymax, css_w, css_h)) = scales
        else {
            return;
        };
        if plot_r <= plot_l {
            return;
        }
        // Nearest sample by rendered position: reuse the downsampled series
        // cached by `sync` (no per-move clone + downsample).
        let best = LAST_DS.with(|c| {
            let g = c.borrow();
            let (ds, window) = g.as_ref()?;
            let metric = METRICS.iter().find(|m| m.key == key.as_str())?;
            // Same y transform the chart renders with, so the selected point is
            // the one whose rendered position is closest to the pointer (2D
            // spatial proximity, not abscissa only).
            let y_of = |v: f64| {
                if ymax == ymin {
                    (plot_t + plot_b) / 2.0
                } else {
                    plot_t + (ymax - v) / (ymax - ymin) * (plot_b - plot_t)
                }
            };
            let mut best: Option<(f64, i64, f64)> = None;
            for s in ds.iter() {
                if let Some(v) = MetricDef::value_of(metric.key, s) {
                    let px = x_of_t(plot_l, plot_r, xmin, xmax, s.ts.0);
                    let py = y_of(v);
                    let d = (px - x).hypot(py - y);
                    if best.is_none_or(|(bd, _, _)| d < bd) {
                        best = Some((d, s.ts.0, v));
                    }
                }
            }
            best.map(|(d, ts, v)| (d, ts, v, *window))
        });
        let Some((d, ts, v, window)) = best else {
            Chart::hide_tip(&key);
            return;
        };

        if d > SNAP_PX && ptype != "mouse" {
            // Touch: require a snap so a stray swipe doesn't pin the tip.
            Chart::hide_tip(&key);
            return;
        }
        let px = x_of_t(plot_l, plot_r, xmin, xmax, ts);
        let with_wd = window > DAY_MS;
        let Some(metric) = METRICS.iter().find(|m| m.key == key.as_str()) else {
            return;
        };
        let y_of = |v: f64| {
            if ymax == ymin {
                (plot_t + plot_b) / 2.0
            } else {
                plot_t + (ymax - v) / (ymax - ymin) * (plot_b - plot_t)
            }
        };
        let py = y_of(v);
        let tip_html = format!(
            "<span class=\"tip-val\">{val} {}</span><br><span class=\"tip-time\">{t}</span>",
            metric.unit,
            val = metric.fmt_value(v),
            t = state::fmt_chart_time(ts, with_wd),
        );
        let tip_left = (px).clamp(30.0, css_w - 30.0);
        // Pin the popup to the data point (like the old Chart.js tooltip): a
        // few css px above it, or just below it when it would clip the top of
        // the box.
        let tip_top = if py < 44.0 {
            (py + 10.0).max(2.0)
        } else {
            (py - 8.0).max(2.0)
        };
        let tip_top = tip_top.min((css_h - 4.0).max(2.0));
        with_charts(|charts| {
            let Some(chart) = charts.get_mut(&key) else {
                return;
            };
            chart.tip_ts = ts;
            chart.tip_val = v;
            chart.tip_px = px;
            dom::set_inner_html(&chart.tip, &tip_html);
            dom::set_style(&chart.tip, "left", &format!("{:.0}px", tip_left));
            dom::set_style(&chart.tip, "top", &format!("{:.0}px", tip_top));
            dom::remove_class(&chart.tip, "hidden");
        });
    }

    fn hide_tip(key: &str) {
        with_charts(|charts| {
            if let Some(chart) = charts.get_mut(key) {
                dom::add_class(&chart.tip, "hidden");
                chart.tip_px = -1.0;
            }
        });
    }
}

/// Auto-enable/disable metrics that the user has not explicitly toggled:
/// a metric with no data in the current window is hidden (its toggle button
/// is unchecked too), and one with data is shown. A user-toggled metric is
/// left as the user set it.
fn auto_enable() {
    for m in METRICS {
        if state::user_toggled(m.key) {
            continue;
        }
        let on = state::has_data(m.key);
        if on != state::is_enabled(m.key) {
            state::set_enabled(m.key, on);
            // `metric-toggles` is an element *id*, not a class (`class="seg"`).
            let btns = dom::query_all("#metric-toggles button");
            for i in 0..btns.length() {
                if let Some(b) = btns.item(i) {
                    let b = b.unchecked_into::<web_sys::Element>();
                    if dom::attr(&b, "data-m").as_deref() == Some(m.key) {
                        if on {
                            dom::add_class(&b, "active");
                        } else {
                            dom::remove_class(&b, "active");
                        }
                    }
                }
            }
        }
    }
}

/// Drop charts whose metric is no longer enabled (removes the DOM too).
fn drop_disabled() {
    let to_remove: Vec<String> = with_charts_ref(|charts| {
        charts
            .keys()
            .filter(|k| !state::is_enabled(k.as_str()))
            .cloned()
            .collect()
    });
    with_charts(|charts| {
        for k in to_remove {
            if let Some(chart) = charts.remove(&k)
                && let Some(wrap) = chart
                    .canvas
                    .parent_element()
                    .and_then(|p| p.parent_element())
            {
                wrap.remove();
            }
        }
    });
}

/// Hide every chart's tooltip (e.g. when the charts are no longer
/// visible): a tooltip pinned by a touch tap must not survive a tab switch.
pub fn hide_all_tips() {
    with_charts(|charts| {
        for ch in charts.values_mut() {
            dom::add_class(&ch.tip, "hidden");
            ch.tip_px = -1.0;
        }
    });
}

/// (Re)draw every enabled chart from the current sample window.
pub fn sync() {
    auto_enable();
    drop_disabled();
    // Downsample once per sync (not once per chart / per clone).
    state::with_state(|s| {
        let ds = downsample(&s.samples, MAX_POINTS).into_owned();
        set_last_ds(ds, s.window);
    });
    let empty = LAST_DS.with(|c| c.borrow().as_ref().is_none_or(|(ds, _)| ds.is_empty()));
    if empty {
        return;
    }
    for m in METRICS {
        if !state::is_enabled(m.key) {
            continue;
        }
        LAST_DS.with(|c| {
            let g = c.borrow();
            let Some((ds, window)) = g.as_ref() else {
                return;
            };
            let data: Vec<(i64, f64)> = ds
                .iter()
                .filter_map(|s| MetricDef::value_of(m.key, s).map(|y| (s.ts.0, y)))
                .collect();
            if data.is_empty() {
                return;
            }
            let window = *window;
            with_charts(|charts| {
                let chart = charts
                    .entry(m.key.to_string())
                    .or_insert_with(|| Chart::new(m.key, &format!("{} ({})", m.label, m.unit)));
                draw(chart, m, &data, window);
            });
        });
    }
}

/// Redraw without (re)filtering; used on window resize.
fn redraw_all() {
    state::with_state(|s| {
        let ds = downsample(&s.samples, MAX_POINTS).into_owned();
        set_last_ds(ds, s.window);
    });
    let empty = LAST_DS.with(|c| c.borrow().as_ref().is_none_or(|(ds, _)| ds.is_empty()));
    if empty {
        return;
    }
    let keys: Vec<String> = with_charts_ref(|charts| charts.keys().cloned().collect());
    for key in keys {
        let Some(m) = METRICS.iter().find(|m| m.key == key.as_str()) else {
            continue;
        };
        LAST_DS.with(|c| {
            let g = c.borrow();
            let Some((ds, window)) = g.as_ref() else {
                return;
            };
            let data: Vec<(i64, f64)> = ds
                .iter()
                .filter_map(|s| MetricDef::value_of(m.key, s).map(|y| (s.ts.0, y)))
                .collect();
            if data.is_empty() {
                return;
            }
            let window = *window;
            with_charts(|charts| {
                if let Some(chart) = charts.get_mut(&key) {
                    chart.resize();
                    draw(chart, m, &data, window);
                }
            });
        });
    }
}

/// Window `resize` handler (simple, mobile-friendly; no ResizeObserver).
/// The returned `Function` must be kept alive for the page lifetime (the
/// `window.onresize` assignment alone does not hold a Rust reference).
pub fn watch_resizes() -> js_sys::Function {
    let f = dom::closure(|_e| redraw_all());
    dom::window().set_onresize(Some(&f));
    f
}

/// Bucketed downsample (ported from `charts.ts`): per bucket keep the last
/// `ts`/`energy`, average the present `power`/`voltage`/`temperature`.
/// Borrows when no bucketing is needed (no copy on the common path).
pub fn downsample<'a>(
    samples: &'a [MeterValueSelection],
    max: usize,
) -> Cow<'a, [MeterValueSelection]> {
    if samples.len() <= max {
        return Cow::Borrowed(samples);
    }
    let bucket = samples.len().div_ceil(max);
    let mut out = Vec::with_capacity(samples.len() / bucket + 1);
    for part in samples.chunks(bucket) {
        let last = part.last().expect("chunk non-empty");
        let mut psum = 0.0;
        let mut pn = 0;
        let mut vsum = 0.0;
        let mut vn = 0;
        let mut tsum = 0.0;
        let mut tn = 0;
        for mv in part {
            if let Some(p) = mv.power {
                psum += p as f64;
                pn += 1;
            }
            if let Some(v) = mv.voltage {
                vsum += v as f64;
                vn += 1;
            }
            if let Some(t) = mv.temperature {
                tsum += t as f64;
                tn += 1;
            }
        }
        out.push(MeterValueSelection {
            ts: last.ts,
            energy: last.energy,
            power: (pn > 0).then(|| (psum / pn as f64).round() as u32),
            voltage: (vn > 0).then(|| (vsum / vn as f64).round() as u16),
            temperature: (tn > 0).then(|| (tsum / tn as f64).round() as u16),
        });
    }
    Cow::Owned(out)
}

/// Map a timestamp to css-px x within the plot area (shared by `draw` and
/// the pointer scrub).
fn x_of_t(plot_l: f64, plot_r: f64, xmin: i64, xmax: i64, t: i64) -> f64 {
    if xmax == xmin {
        return plot_l;
    }
    plot_l + (t as f64 - xmin as f64) / (xmax as f64 - xmin as f64) * (plot_r - plot_l)
}

fn draw(chart: &mut Chart, m: &MetricDef, data: &[(i64, f64)], window: i64) {
    let ctx = match chart.ctx() {
        Some(c) => c,
        None => return,
    };
    let w = chart.css_w;
    let h = chart.css_h;
    let _ = ctx.set_transform(chart.dpr, 0.0, 0.0, chart.dpr, 0.0, 0.0);
    ctx.clear_rect(0.0, 0.0, w, h);
    dom::add_class(&chart.tip, "hidden");
    if data.len() < 2 {
        return;
    }
    let x0 = data[0].0;
    let x1 = data[data.len() - 1].0;
    let xmin = x0 - X_SLACK_MS;
    let xmax = x1 + X_SLACK_MS;

    // Y range.
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for (_, y) in data {
        ymin = ymin.min(*y);
        ymax = ymax.max(*y);
    }
    if m.begin_at_zero {
        ymin = ymin.min(0.0);
    }
    if ymin == ymax {
        ymin -= 1.0;
        ymax += 1.0;
    }
    // Headroom (both ends, or top-only for beginAtZero).
    let span = ymax - ymin;
    if !m.begin_at_zero {
        ymin -= span * Y_HEADROOM;
    }
    ymax += span * Y_HEADROOM;
    if let Some(smax) = m.suggested_max {
        ymax = ymax.max(smax);
    }

    let plot_l = PAD_L;
    let plot_r = w - PAD_R;
    let plot_t = PAD_T;
    let plot_b = h - PAD_B;
    let x_of = |t: i64| x_of_t(plot_l, plot_r, xmin, xmax, t);
    let y_of = |v: f64| plot_t + (ymax - v) / (ymax - ymin) * (plot_b - plot_t);

    // Remember the x and y scales so pointer scrubbing can snap to data
    // points and place the tooltip at the point's own y.
    chart.xmin = xmin;
    chart.xmax = xmax;
    chart.plot_l = plot_l;
    chart.plot_r = plot_r;
    chart.plot_t = plot_t;
    chart.plot_b = plot_b;
    chart.ymin = ymin;
    chart.ymax = ymax;

    ctx.set_line_width(1.0);

    // Grid + y ticks (4 divisions, like Chart.js defaults).
    ctx.set_font("11px system-ui, sans-serif");
    for i in 0..=4 {
        let v = ymin + (ymax - ymin) * i as f64 / 4.0;
        let y = y_of(v);
        ctx.set_stroke_style_str("rgba(139,152,165,0.15)");
        ctx.begin_path();
        ctx.move_to(plot_l, y);
        ctx.line_to(plot_r, y);
        ctx.stroke();
        ctx.set_fill_style_str("#8b98a5");
        ctx.set_text_align("right");
        ctx.set_text_baseline("middle");
        let _ = ctx.fill_text(&m.tick_label(v), plot_l - 5.0, y);
    }

    let day_start = (xmin / DAY_MS) * DAY_MS;
    for i in 0..TICKS {
        let t = xmin as f64 + (xmax as f64 - xmin as f64) * i as f64 / (TICKS - 1) as f64;
        let x = x_of_t(plot_l, plot_r, xmin, xmax, t as i64);
        ctx.set_stroke_style_str("rgba(139,152,165,0.15)");
        ctx.begin_path();
        ctx.move_to(x, plot_t);
        ctx.line_to(x, plot_b);
        ctx.stroke();
        let label = if window <= DAY_MS {
            state::fmt_chart_time(t as i64, false)
        } else if t as i64 >= day_start && (t as i64 - day_start) < X_SLACK_MS {
            state::fmt_chart_time(t as i64, true)
        } else {
            String::new()
        };
        if !label.is_empty() {
            ctx.set_fill_style_str("#8b98a5");
            ctx.set_text_align("center");
            ctx.set_text_baseline("top");
            let _ = ctx.fill_text(&label, x, plot_b + 5.0);
        }
    }

    // The line. `data` holds only present values, so each run of consecutive
    // timestamps is one span; a time jump marks a span break (spanGaps).
    let mut first: Option<(f64, f64)> = None;
    let mut last_pt: Option<(f64, f64)> = None;
    ctx.set_line_width(1.5);
    ctx.set_line_join("round");
    ctx.set_stroke_style_str(m.color);
    ctx.begin_path();
    let mut prev_t: Option<i64> = None;
    for (t, v) in data {
        let (x, y) = (x_of(*t), y_of(*v));
        match first {
            None => {
                ctx.move_to(x, y);
                first = Some((x, y));
            }
            Some(_) => {
                // A large time gap means the metric was missing in between.
                if let Some(pt) = prev_t {
                    if *t - pt > SPAN_GAP_MS {
                        ctx.move_to(x, y);
                    } else {
                        ctx.line_to(x, y);
                    }
                } else {
                    ctx.line_to(x, y);
                }
            }
        }
        prev_t = Some(*t);
        last_pt = Some((x, y));
    }
    ctx.stroke();

    // Subtle fill under the line, down to the plot bottom.
    if let (Some((sx, _)), Some((ex, _))) = (first, last_pt) {
        ctx.set_fill_style_str(m.color);
        ctx.set_global_alpha(0.13);
        ctx.begin_path();
        for (i, (t, v)) in data.iter().enumerate() {
            let (x, y) = (x_of(*t), y_of(*v));
            if i == 0 {
                ctx.move_to(x, y);
            } else {
                ctx.line_to(x, y);
            }
        }
        ctx.line_to(ex, plot_b);
        ctx.line_to(sx, plot_b);
        ctx.close_path();
        ctx.fill();
        ctx.set_global_alpha(1.0);
    }
}

impl MetricDef {
    /// Y-axis tick label: whole numbers for `integer` metrics and for large
    /// values, otherwise 1 decimal.
    pub fn tick_label(&self, v: f64) -> String {
        if self.integer || v.abs() >= 100.0 {
            format!("{v:.0}")
        } else {
            format!("{v:.1}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_system_common_model::model::Ts;
    use web_sys::Element;

    fn sample(t: i64) -> MeterValueSelection {
        MeterValueSelection {
            ts: Ts(t),
            energy: Some(t as u64),
            power: Some(5_000),
            voltage: Some(230),
            temperature: Some(25),
        }
    }

    fn metric(key: &str) -> &'static MetricDef {
        METRICS.iter().find(|m| m.key == key).unwrap()
    }

    #[test]
    fn tick_labels_integer_metrics_have_no_decimals() {
        assert_eq!(metric("voltage").tick_label(230.4), "230");
        assert_eq!(metric("temp").tick_label(25.0), "25");
        assert_eq!(metric("temp").tick_label(0.0), "0");
    }

    #[test]
    fn tick_labels_fractional_metrics_keep_the_old_rule() {
        assert_eq!(metric("power").tick_label(3.5), "3.5");
        assert_eq!(metric("power").tick_label(120.0), "120");
        assert_eq!(metric("energy").tick_label(12.34), "12.3");
    }

    #[test]
    fn tooltip_value_format() {
        assert_eq!(metric("voltage").fmt_value(230.0), "230");
        assert_eq!(metric("temp").fmt_value(24.0), "24");
        assert_eq!(metric("power").fmt_value(3.5), "3.500");
        assert_eq!(metric("energy").fmt_value(12.25), "12.250");
    }

    /// Regression: the scrub popup is a `div`, not a canvas. Creating it as an
    /// `HtmlCanvasElement` made `dyn_into` fail and panicked the whole app on
    /// the first Metrics-tab render (wedge: `CHARTS` left borrowed).
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn chart_creation_builds_tip_div() {
        let c = Chart::new("test", "Test");
        let tip = c.tip.unchecked_ref::<Element>();
        assert_eq!(tip.tag_name(), "DIV");
        assert!(tip.class_list().contains("chart-tip"));
        assert!(tip.class_list().contains("hidden"));
        // Keep the test self-contained: leave #charts and CHARTS as found.
        if let Some(wrap) = c.canvas.parent_element().and_then(|p| p.parent_element()) {
            wrap.remove();
        }
        with_charts(|charts| {
            charts.remove("test");
        });
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn chart_sync_renders_enabled_metrics() {
        state::with_state(|s| {
            s.power = true;
            s.energy = false;
            s.voltage = false;
            s.temp = true;
            s.samples = (0..20).map(sample).collect();
        });
        sync();
        with_charts_ref(|charts| {
            assert_eq!(
                charts.keys().map(String::as_str).collect::<Vec<_>>(),
                vec!["power", "temp"]
            );
            for c in charts.values() {
                assert_eq!(
                    c.canvas.parent_element().map(|p| p.tag_name()),
                    Some("DIV".to_string())
                );
            }
        });
        // Keep the test self-contained: leave #charts and CHARTS as found.
        drop_disabled();
        state::with_state(|s| {
            s.samples.clear();
            s.power = true;
            s.temp = true;
        });
    }
}
