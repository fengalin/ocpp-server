# WASM frontend (webserver + front end)

> Status: done. The TS/Chart.js frontend is gone; `crates/web-app` (Rust →
> wasm) shares the exact wire types with the server via
> `charge-system-web-model` / `charge-system-common-model`. Scope of this doc
> is `src/webserver/`, the front end, and the wire model.

## Goal
Replace the hand-mirrored TypeScript/Chart.js frontend with a **Rust →
WebAssembly** frontend that **shares the exact same Rust types** as the
server, so there is no more mapping between a Rust model and a TS model.
Serialization is unified on **JSON for every channel** (REST + SSE).
(MessagePack was tried during the migration and reverted: base64 for SSE
wiped out its size advantage, and binary bodies are painful to debug.)
Metric graphs use a **hand-rolled Rust canvas plotter** (no Chart.js).

The OCPP server already uses `ocpp_rs` + `tokio` + `axum`. The frontend is a
separate crate compiled to wasm and served from `static/` exactly like the old
`app.js`.

## Non-negotiables (kept from the mockup)
- **SSE stays** (`/api/stream`) with the snapshot-then-catch-up protocol: a
  (re)connect runs a *serial* refresh (`GET /api/state` → `GET /api/events` →
  `GET /api/metrics?from&to`), and steady state is **fully event-driven** —
  SSE events are applied locally, nothing polls (see "Frontend update
  protocol"). Rationale: phone radio wake-ups, phone-sleep reconnect,
  auto-reconnect. Transport-agnostic.
- **Plans**: the web app previews locally with `common_model::plan::compute`;
  applying a plan only sends a command to the EVSE — what is shown afterwards
  is what the EVSE notifies (`Notification::ChargingPlan`), so the UI can
  never drift from the applied state.
- Terminology, units, and SoC convention stay (see "Key differences" below).
- Responsive, mobile-first, no framework (plain `web-sys` + `wasm-bindgen`).

## Key differences: real OCPP server vs the mockup
These are the "type hops" and semantic changes the maintainer flagged.

1. **SoC is a *fraction* 0–1 (f32), not a percent 0–100.** The mockup used
   `f32` percent everywhere. The real server's `Bms`/`SoC` are full-precision
   fractions (`bms.rs`: `SoC::Absolute(f32)` where the value is a ratio,
   `Display` multiplies by 100). The web model, plan math, and validation all
   use fractions (`Bms.current_soc`, `Plan.target_soc`,
   `battery_energy_needed`, `validate_plan`). **The frontend
   renders `* 100` and the plan request sends a fraction** (the old TS
   `main.ts`/`plan.ts` already did `* 100` — carried over). Fractions are
   rounded to 3 decimals at the notification boundary (`evse.rs`) before they
   reach the wire.
2. **The sample type is `charge_system_common_model::MeterValueSelection`**,
   not a `Sample` struct. It has **all-Option numeric fields**: `ts: Ts`,
   `energy: Option<u64>` (Wh), `power: Option<u32>` (W),
   `voltage: Option<u16>` (V), `temperature: Option<u16>` (°C). It is
   serde-only (the OCPP `From` impl sits behind common_model's `ocpp`
   feature), so it lives in `common_model`; `measurements.rs` just imports
   it — no re-export — see "Shared crate layout". The `meter_values` SSE
   event carries a single `MeterValueSelection`.
3. **No simulator.** `SimState` is gone. `GET /api/state` has no `sim` block;
   `ServerState` = `{ ts, bms: Bms, status: ChargeStatus,
   charging_plan: Option<AppliedChargingPlan> }`, where
   `charging_plan` is the plan currently in effect on the EVSE plus its
   computed schedule (`AppliedChargingPlan { plan, periods }`, recorded from
   `Notification::ChargingPlan`; `Unlimited` maps to `None`, every other plan
   — including the default post-session `Blocking` — is shown on the status
   tab's plan card). The computed
   start time only exists server-side
   (`ChargingSchedule::try_from_charging_plan`), so it is
   sent rather than re-derived client-side. The charge
   point's live status is the
   `ChargeStatus` from the unix-socket wire contract — `Charging(_)` /
   `SuspendedEvse(_)` carry a `ChargeProgress` (live SoC + remaining time)
   that both the server (recorder) and the frontend apply to their BMS copy
   (current SoC). The target SoC is not carried here — it already rides on
   `BmsUpdate` / the charging-plan notification. The BMS `current_energy` is
   kept fresh from **meter values** carrying a defined energy, so the status
   tab's *Session energy* tile (`current_energy − initial_energy`) stays
   current between/without samples and survives reconnect.
4. **`Characteristics` is gone — resolved.** The BMS model (`Bms`, `SoC`,
   `SoCProgress`) now lives in `common_model::bms` and the plan computation
   takes `&Bms` everywhere (server, web app, mock).
5. **Charging-plan notifications are wired.** The SSE stream carries raw
   `Notification`s (no webserver relay — the endpoint subscribes to the shared
   notification channel directly), and `evse.rs` (`set_charging_plan`)
   broadcasts
   `Notification::ChargingPlan(AppliedChargingPlan)` — the requested plan plus
   the schedule computed by the EVSE (`ChargingSchedule::try_from_charging_plan`, same shared
   math as previews); a plan-less / non-computable state is sent as
   `{ plan: Unlimited, periods: [] }`. The EVSE applies the default plan on its
   own on every Finishing → Available transition (notifying
   `{ Blocking, [] }` — the CP forgets its profile on EV disconnect, and this
   keeps a fresh plug-in from starting an unmanaged session), and
   `/api/plans/stop` re-applies `Blocking` over whatever is active. The
   webserver's world, the web app and the OLED app all update from that single
   event; the plan's target SoC *is* the BMS target (Deadline/Asap set it,
   every other plan clears it — no separate `BmsUpdate` on the plan path).
   `web_model::StreamEvent` was dropped with the relay. **Breaking for the
   OLED app:** the shared variant `NoLimit` is now `Unlimited` (the JSON wire
   string changed) and `Blocking` is a server-applied state.
6. **Endpoints differ from the mockup**: `POST /api/cp/set_meter_value_interval`
   (body `{ interval: u16 }`), `POST /api/cp/reboot` (no `cp/reset`). SSE event
   names: `bms_update`, `status`, `connection`, `meter_values`,
   `charging_plan` (each a raw `Notification`; + synthetic `resync` on lag).
   The `status` event payload is the `ChargeStatus` itself (the webapp's
   notion of "session active" is
   `Preparing | Charging(_) | SuspendedEvse(_)`; the status-tab stop-session
   button is enabled exactly in those states — a Preparing session can be
   stopped, the server sends `RemoteStopTransaction`).

## Architecture

### Shared crate layout
The wire types moved out of `src/webserver/model.rs` into two shared crates
that both the server (rlib) and the wasm frontend (cdylib) compile against;
`model.rs` itself is gone — call sites import from the crates directly.

- New crate `charge-system-common-model` (path dep, `crates/common_model/`)
  holds the types shared with **external** consumers (the WASM web app *and* the
  OLED status app): `Ts`, `MeterValueSelection` (+ its OCPP conversion, behind
  the `ocpp` feature), and the unix-socket wire contract
  (`Notification`, `ChargeStatus`, `ChargeProgress`,
  `ChargePointConnectionState`). Deps: `serde`, `chrono` (`ocpp_rs` only
  behind the `ocpp` feature; `serde_json` dev-dep for tests) — **no
  `ocpp_rs`/`rusqlite`** in the default build, so it is wasm-safe.
- New crate `charge-system-web-model` (path dep, `crates/web_model/`) holds the
  **webapp-only wire types**: `ServerState`, `PlanRequest` (+ ring limits);
  the SSE events themselves are `common_model::Notification`s. It depends on
  `common_model` but does **not** re-export anything — every call site
  imports shared types directly from `charge-system-common-model` (no
  re-export modules, by convention).
- `ChargingPlan` (the EVSE/CLI plan model) was **relocated** from server
  `schedule.rs` into `common_model::plan_model`: it now backs
  `Notification::ChargingPlan`, so the web app and the OLED app can consume
  it. The OCPP schedule computation stays server-side, as
  `schedule::ChargingSchedule::try_from_charging_plan` — an inherent method
  on the server-local `ChargingSchedule`, since `ChargingPlan` lives in
  `common_model` (the server crate can't add inherent impls to a foreign
  type). An `Asap { target_soc, power_limit }` variant was added for
  the web app's start-now kind. Since: `NoLimit` was renamed **`Unlimited`**
  (breaking — see above), `PlanKind` gained `Blocking` / `Unlimited`, and
  `PlanRequest`'s options became kind-scoped (`target_soc` / `power_limit`
  are `Option<…>`, `None` for the option-less kinds) — the web form's mode
  radios were replaced by a 4-way kind chip row (Deadline / ASAP / Blocking /
  Unlimited; the last two hide all option fields), and applying over an
  active Deadline/Asap plan asks for confirmation.
- `MeterValueSelection` is **relocated** from `src/measurements.rs` into
  `common_model`; `measurements.rs` keeps its `MeterValueObserver`
  consolidation logic and all tests, importing the type directly. The
  `From<&ocpp_rs MeterValue>` impl lives in `common_model` behind the `ocpp`
  feature.
- `src/webserver/model.rs` is gone; its types now come from
  `charge-system-web-model`. `api.rs`, `state.rs`, `world.rs` import
  from there.

> Step 1 did the epoch-millis + MessagePack switch **inside
> `webserver/model.rs`** (no crate split) so the existing TS frontend still
> worked end-to-end and the wire format was proven. Step 2 split it into
> `charge-system-common-model` + `charge-system-web-model` + relocated
> `MeterValueSelection` when the wasm crate was added.

### Time encoding: epoch millis (`i64`)
Every `DateTime<Utc>` in a wire type is serialized as **`i64` milliseconds
since epoch**. This (a) is what makes the types cross `wasm-bindgen` cleanly
(chrono types can't cross the boundary), and (b) shrinks every sample.

Implementation: a small `Ts(i64)` newtype (or `#[serde(with = ...)]` module)
used in the wire structs, with `From<DateTime<Utc>>`/`From<i64> for
DateTime<Utc>` and `.as_ms()`. Keep `DateTime<Utc>` as the *internal* type in
server-side code; only the wire boundary converts.

### Serialization: JSON everywhere
> **Update:** REST + SSE were briefly moved to `rmp-serde` MessagePack during
> the migration (SSE base64-encoded, since SSE `data:` fields are text) for
> compactness. Reverted to plain JSON: with the base64 overhead the payloads
> were not meaningfully smaller, and binary bodies are hard to inspect in
> devtools. The wire contract is JSON (`application/json`) on REST and in SSE
> `data:` fields (`Notification::to_sse_data`); error responses were always JSON.
>
> wasm has no bearing on this — `serde_json` compiles fine to
> wasm32-unknown-unknown.

`Option<u32>`/`Option<u16>`/`Option<u64>` (the `MeterValueSelection` fields)
must **not** use `#[serde(skip_serializing_if)]`: every field is written
(`null` when absent) so clients never have to guess a missing one
(see `meter_value.rs`).

### Frontend crate (`ui/` → `web-app/`)
- Crate `web-app` at `crates/web-app/`, `crate-type = ["cdylib"]`. It is a
  regular member of the root workspace (the editor's rust-analyzer wants one
  workspace root); its size-optimized release build uses the named
  `release-slim` workspace profile (`wasm-pack build --profile
  release-slim`), since cargo has no per-package profiles. Deps: `wasm-bindgen`, `wasm-bindgen-futures`, `web-sys` (features:
  `Window`, `Document`, `Element`, `HtmlElement`, `HtmlCanvasElement`,
  `CanvasRenderingContext2d`, `EventSource`, `Event`, `MessageEvent`,
  `KeyboardEvent`, `HtmlInputElement`, `HtmlButtonElement`, `console`, …),
  `js-sys` (fetch + JSON go through `web_sys`/`js_sys` — no `gloo-net`),
  `charge-system-web-model` + `charge-system-common-model` (the shared types),
  `serde`/`serde_json`, `chrono` (epoch-millis → local formatting).
- Output: `wasm-pack build --target web --out-dir ../../static/wasm
  --profile release-slim` →
  `static/wasm/web_app.js` + `web_app_bg.wasm`. `index.html` loads
  `wasm/web_app.js`.

### DOM strategy
No framework. A small helper module wraps `web-sys` for the patterns already
used in the TS (`$("id")`, `innerHTML` for list rendering, `classList`,
`addEventListener`). Element ids stay stable across reworks; `static/index.html`
and `static/styles.css` have since evolved with the plan form (kind chip row,
SoC inputs + presets) and the stop button moving into the status badge.

### Canvas plotter (replaces Chart.js)
Port the existing behavior 1:1 (`ui/src/charts.ts`):
- One line chart per enabled metric (power/energy/voltage/temperature).
- Downsample to ≤ 700 points (bucketed: last `ts`/`energy`, mean of present
  `power`/`voltage`/`temperature`).
- Linear **x = epoch-ms** axis (no date adapter), **spanGaps**: a `None`
  value breaks the line (move_to on the next present point).
- Window tick formatting: ≥ 6 h shows weekday + HH:MM, else HH:MM; ~6 ticks.
- y `beginAtZero` for power, `suggestedMax ≈ 8 kW` for power.
- Fixed colors per metric (power `#4cc38a`, energy `#5aa9e6`, voltage
  `#e6b45a`, temp `#e0705a`), 1.5 px line, subtle fill.
- Responsive: canvas sized to its container; redraw on window resize + on
  data change. `devicePixelRatio` scaling for crisp lines.
- Live: append on SSE `sample` and redraw event-driven (there is **no periodic
  tick** in the app); the auto-enable rule ("metric with no data in the window
  is hidden") runs on every plot sync but never touches a metric the user
  explicitly toggled.

This is a ~250–350 line module. It's the most "new" code but is a straight
port of logic that already exists and is well understood.

## Frontend update protocol (current)

The front end has **no periodic timers** — no poll, no 1 s or 60 s tick.
Everything is driven by SSE events and user actions; the only timer that ever
runs is the adaptive remaining-time countdown (below). All of it lives in
`ui/catchup.rs` + `ui/plan.rs`.

**Boot / (re)connect — serial refresh.** `init()` first restores persisted
prefs (`localStorage`, see below), wires the UI, opens the SSE stream, then
runs one serial refresh: `GET /api/state` → `GET /api/events` →
`GET /api/metrics?from&to`. Each step is applied + rendered as it lands and
fails independently (the sample fetch still runs if events fail). The same
refresh runs on SSE open, on the synthetic `resync` event (server-side lag),
after plan apply/stop, session stop, CP actions — a latest-wins flag prevents
the boot call from doubling with the one triggered by the SSE open. A metrics
timeframe chip refreshes **only the sample window** (`RefreshScope::SamplesOnly`).

**Steady state — event-driven.** SSE `meter_values` appends one sample (and
keeps the BMS copy's `current_energy` fresh when it carries a reading,
re-syncing the plots when the metrics tab is active), `status` appends a
client-side log entry + updates the session badge/BMS SoC, `bms_update`
merges into `ServerState.bms`, `charging_plan` applies the
`AppliedChargingPlan` locally
(status-tab card, BMS target reseed, and mirroring mode/deadline/power limit
into the plan form — a stop never clobbers a draft). None of them fetch REST;
the only exception is when a `charging_plan` event races ahead of the very
first `/api/state` snapshot (page just opened), in which case a full refresh
runs instead of dropping it.
The header clock shows the *last message received* from the server (a
freshness indicator), not the wall clock.

**Event log.** The client reconstructs the log itself: every `status` event
appends an entry timestamped with the client clock, mirroring the server's
retention rule (`MAX_EVENTS = 32`) — a charging phase keeps its **first +
latest** entry (at most two consecutive `Charging` rows), every other status
always appends. A refresh replaces the log with the server's authoritative
history (`webserver/world.rs` and the client's `push_event_local`
 implement the same rule).

**Link dot.** Three states from the SSE lifecycle: gray *Connecting* → green
*Live* on open, red *Disconnected — retrying* on error (the browser
`EventSource` reconnects natively; the reopen fires a full refresh). The
header re-renders immediately on each transition (`render_header`), since no
tick would pick it up.

**Remaining-time countdown.** The app's only timer, re-baselined from
`ChargeProgress.remaining_time` — present both in `status` SSE events and in
the `/api/state` snapshot: 60 s tick while ≥ 3 min remain, 1 s below;
`H h MM min` / `M min` above the threshold, `MM:SS` below. Re-armed on every
re-baseline, cancelled at zero. It shows *charging time* still to come — not
the wall-clock distance to the schedule end: before a plan starts it is the
whole window (schedule end − start), then its remaining part once the window
has begun; nothing is shown after the end. During a live session the charge
progress stays authoritative (the schedule-derived value only fills in when
none is set).

**Prefs (`localStorage`).** The active tab, metrics window and the four graph
toggles persist across reloads under `charge-system.web-app.prefs`; a
restored selection counts as *user-toggled*, so the plot auto-enable rule can
never override it.

**Gotcha — the metric toggle buttons live in `#metric-toggles` (an element
*id*, `class="seg"`).** Any code that syncs their `active` class must query
`#metric-toggles button`, not `.metric-toggles button`.

## Build & run (target state)
```
cargo build                       # server (unchanged command)
# frontend:
pushd crates/web-app && wasm-pack build --target web \
  --out-dir ../../static/wasm --profile release-slim && popd
cargo run -- run                 # serves static/ (incl. wasm/) via ServeDir
```
Dev: `wasm-pack build --dev` + `watch` via `cargo watch` on the crate.

## Out of scope (flagged as comments, maintainer handles)
- The `ChargingPeriod.duration` vs `end_at` naming (TS had a FIXME about it).
- `ocpp_rs`/`rusqlite` in the wasm dependency graph (must stay out).

## Risks / gotchas
- **wasm-opt & bulk memory**: some release-profile knob combinations
  (e.g. `strip = "symbols"`) make rustc emit bulk-memory ops (`memory.copy`);
  wasm-pack's wasm-opt invocation then fails (it passes no
  `--enable-bulk-memory`, and this wasm-pack version only accepts a bool for
  the `wasm-opt` metadata). `release-slim` (opt-level "s", fat LTO,
  codegen-units 1, no strip) is known-clean; if you change it, rebuild via
  wasm-pack to check.
- **`u64` → JS BigInt**: `MeterValueSelection.energy: u64` crosses
  wasm-bindgen as `BigInt`.
  Prefer `i64`/`f64` on the *JS-facing* accessor surface, or convert inside
  the crate. (The JSON wire handles `u64` fine; this is only a wasm-bindgen
  boundary concern.)
- **Panic guards**: a panic in a wasm event/SSE handler unwinds through the JS
  boundary and would kill the whole instance (any live `RefCell` borrow stays
  wedged — the page loads but nothing works, no console output). Every
  listener is wrapped (`dom::guard_panic` / `api::ev_c`), the panic hook
  reports to the console **and** on screen, and `init().catch()` surfaces
  startup failures.
- **The stop glyph is ■ (U+25A0)**, not ⏹: the latter has an emoji
  presentation that some platforms render as a fixed-color glyph ignoring the
  CSS color.
- **`f32` SoC precision** is fine (values ~0–1; rounded to 3 decimals at the
  notification boundary).
- **SSE `Event::data`** is a JSON string (`Notification::to_sse_data`); the
  browser `EventSource` delivers it as a UTF-8 string and the frontend parses
  it with `serde_json` — no binary encoding involved.
- **Static dir**: `ServeDir::new("static")` already serves any file; adding
  `static/wasm/` needs no server change.
- **Cross-compile**: the server has a `Cross.toml` (ARM target for the CP
  host). The wasm frontend is built natively (x86_64 → wasm32), not via
  cross — keep the two build paths separate.
