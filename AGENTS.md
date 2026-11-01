# AGENTS.md

The charge system is a Rust OCPP 1.6 server for a home EV charge point (CP).
The `charge-system-server` crate talks OCPP to the CP over a websocket
(`ocpp_rs`), keeps charging sessions and schedules in SQLite (`rusqlite`),
hosts a **web app** (REST + SSE + static files) under `crates/server/src/webserver/`,
and pushes unix-socket notifications to a local OLED status app.

> **Reference: WASM frontend.** Read `docs/wasm-frontend.md` before touching
> `crates/server/src/webserver/`, the front end (`crates/web-app`), or the
> model/serialization.

## Crates
- `crates/charge-system-server` (dir `server/`) — the binary: OCPP client,
  charging sessions, schedules, the webserver, and the unix-socket notifier.
  CLI: `run` starts the server; the other subcommands are one-shot OCPP
  actions that connect, do their thing, and exit (`reboot`, `stop-session`,
  `set-server-ip`, …).
- `crates/charge-system-common-model` (dir `common_model/`) — **everything
  shared amongst multiple crates: types AND logic.** Contains `Ts`,
  `MeterValueSelection` (+ its OCPP conversion, `ocpp` feature), the BMS model
  (`bms` module: `Bms`, `SoC`, `SoCProgress`), the unix-socket wire contract
  (`Notification` — with its SSE helpers `name()`/`to_sse_data()` —,
  `ChargeStatus`, `ChargeProgress`, `ChargePointConnectionState`,
  `UNIX_SOCKET_PATH`), and the
  scheduling/charging-plan computation (`plan` module: `compute`, `live`,
  `estimate_soc`; `plan_model`: `ChargingPeriod`, `ChargingPlan` (the EVSE/
  CLI plan model, incl. `Asap`), `AppliedChargingPlan` (a requested plan +
  its computed schedule periods — the payload of
  `Notification::ChargingPlan`), `Plan`, `PlanPreview`, `LivePlan`) needed
  by the server and the web app. Must stay buildable
  standalone (own `version`/`edition`, no workspace inheritance) — it is
  pulled by the OLED repo via git.
- `crates/charge-system-web-model` (dir `web_model/`) — **webapp-only** types
  & logic: the web/HTTP+SSE wire contract: `ServerState`, `PlanRequest`
  (the SSE events themselves are `common_model::Notification`s — there is no
  more `StreamEvent` relay type). Depends on `common_model`; every shared
  type is used directly from there (no re-exports anywhere in this
  workspace).
- `crates/charge-system-web-mock` (dir `web_mock/`) — dev-only harness that
  fakes the server's HTTP surface to smoke-test the front end.
- `crates/web-app/` — the WASM front end. A regular workspace **member**
  (the editor's rust-analyzer indexes the repo as one workspace root, so it
  must not be split out); built to wasm with wasm-pack, see Build & run.

## What the webserver does (crates/server/src/webserver/)
- `mod.rs` — `Webserver`: binds the HTTP port, spawns the recording task
  (`AppState::into_task`) + the axum serve task.
- `api.rs` — REST handlers + SSE `/api/stream` + static serving
  (`ServeDir::new("static")`). Endpoints: `GET /api/state`, `GET
  /api/metrics?from&to`, `GET /api/events?from`, `POST /api/plans/apply`,
  `POST /api/plans/stop`, `POST /api/session/stop`, `POST
  /api/cp/set_meter_value_interval`, `POST /api/bms/init_soc`, `POST
  /api/cp/reboot`, `GET /api/stream`. SSE emits the raw `Notification`s under
  their `name()` (`bms_update`, `status`, `connection`, `meter_values`,
  `charging_plan`); each connection subscribes to the shared notification
  channel directly (no relay), `Lagged` → synthetic `resync` event.
- `state.rs` — `AppState`: `World` (lock) + the shared notification channel's
  **sender** (`notif_tx`) + `cmd_tx` (commands to the CP). The sender is held
  (not a receiver) so the SSE endpoint subscribes per connection
  (`st.notif_tx.subscribe()`) — a request clone must not leave a receiver
  buffering messages nobody reads. `into_task(self, rx)` runs as the
  **recording task**: it drains the receiver handed to it and is the **sole
  recorder** of `World` (survives `Lagged` with a warn + continue), calling
  `record()` per notification. The recording task's receiver is subscribed in
  `Webserver::new` — which `main.rs` runs *before* the dispatcher — so the
  startup `BmsUpdate` from `Evse::new` is not missed.
- `world.rs` — `World`: the webserver's in-memory state (BMS copy, event
  log, meter values, and the plan currently in effect on the EVSE, recorded
  from `Notification::ChargingPlan`). Recording a Deadline/Asap plan also
  mirrors its target SoC onto the world's BMS copy. Event-log retention
  (`push_event`) keeps a charging
  phase's *first + latest* entry (≤ 2 consecutive `Charging`) — the web app
  mirrors this rule client-side.

## Key invariants / conventions
- **SoC is a fraction 0–1 (f32), not a percent.** `Bms`/`SoC`
  (`common_model/src/bms.rs`) store full-precision ratios; `Display`
  multiplies by 100. The web model, plan math,
  and the frontend follow this (the UI renders `* 100`, plan requests send a
  fraction). SoC reaches the web app / OLED app through
  `Notification::BmsUpdate(Bms)` — emitted at startup, on init-SoC set and on
  **session start** (where `initial_energy` changes) — rounded to 3 decimals
  at the notification boundary (`evse.rs`). Setting an initial SoC while the
  current one is unknown (no session energy delivered yet) also sets the
  current SoC to it, so plan creation starts from a real value. During
  charging the live SoC rides
  on status events: `ChargeProgress` in `Charging(_)` / `SuspendedEvse(_)`
  carries only current SoC + remaining time, mirrored into the BMS copy by
  both the webserver recorder and the web app; the target SoC is *not* carried
  there (BmsUpdate / plan notifications own it). The BMS `current_energy` is
  kept fresh from **meter values** that carry a defined energy — both the
  webserver recorder and the web app apply it to their BMS copy. The status
  tab's *Session energy* tile is computed from the BMS (`current − initial`).
  Don't reintroduce percent on the wire.
- **The sample type is `charge_system_common_model::MeterValueSelection`**
  (all-Option numeric fields, `ts: Ts`). It is the EVSE's focused subset of the
  OCPP `MeterValue` (originally for logging), shared by the server, the web app,
  and the OLED app — every crate imports it directly from `common_model`.
  `SSE::Sample` carries one `MeterValueSelection` + `soc: Option<f32>`.
- **Crate split rule:** `charge-system-web-model` holds only what is pertinent
  to the web app; anything shared by multiple crates (types *and* logic —
  e.g. plan/schedule computation) lives in `charge-system-common-model`.
- **No re-export modules / hubs** (user preference, 2026-10-31): every type is
  `use`d from the crate that defines it (e.g. `charge_system_common_model::…`).
  Do not reintroduce convenience `pub use` shims between crates — a type has
  exactly one canonical path.
- **The plan the EVSE notifies is what everyone shows.** The web app previews
  locally with `common_model::plan::compute`; `/api/plans/apply` only sends a
  command to the EVSE (which computes the OCPP schedule from its own BMS), and
  the resulting `Notification::ChargingPlan` — an
  `AppliedChargingPlan { plan, periods }`: the plan actually applied plus its
  computed schedule (`ChargingSchedule::try_from_charging_plan`), or
  `{ Unlimited, [] }`
  when no plan is in effect (an explicit unlimited apply) — updates the world
  and all clients. A plan whose schedule could *not* be computed is replaced
  by the default (Blocking) plan — a failed computation never leaves the CP
  unrestricted — and notified as `Blocking`. The computed
  start time is sent rather than re-derived client-side. No separate plan
  state lives in the webserver. `Blocking` is the *default* plan: the EVSE
  applies (and notifies) it on every Finishing → Available transition, so a
  fresh plug-in never starts an unmanaged session; `/api/plans/stop` also
  replaces whatever is active with `Blocking`. The BMS target SoC mirrors the
  active plan (Deadline/Asap set it, all other plans clear it) — except a
  computation-failure fallback to Blocking keeps the target that request just
  set (what the user entered; not cleared, not rolled back), carried to
  clients by a follow-up `BmsUpdate`. Applying
  `Unlimited` sends `ClearChargingProfile` so any profile installed on the CP
  is removed.
- **SSE + snapshot-then-catch-up** is the live-update protocol (not polling /
  WS). On (re)connect the client does a *serial* refresh — `/api/state` →
  `/api/events` → the visible `/api/metrics` window — and steady state is
  **fully event-driven: the front end has no periodic timers** (the only timer
  ever running is the adaptive remaining-time countdown); see
  `docs/wasm-frontend.md` ("Frontend update protocol"). All wire payloads are
  JSON — a MessagePack experiment was reverted for debuggability.
- Units: energy `u64` **Wh**, power `u32` **W`, voltage `u16` **V**,
  temperature `u16` **°C**, durations `u64` **s**.
- The frontend is the Rust→WASM app in `crates/web-app` (no framework,
  hand-rolled canvas plotter); it shares the exact wire types with the server
  via `web_model`/`common_model` (see `docs/wasm-frontend.md`).

## Build & run
Layout: virtual workspace, all crates flat under `crates/` (rust-analyzer
style). `default-members` = `crates/server`, so plain cargo commands at the root
target the server; use `--workspace` (or `-p`) to reach the other crates.
`charge-system-web-mock` is a dev-only harness (member, but not a default).
```
cargo build                              # = the charge-system-server binary
cargo run -- run                         # run the server (web 9080 / OCPP 9000;
                                         # flags: --webserver-port, --ocpp-port, …)
cargo run -- reboot                      # one-shot OCPP command, then exit
cargo test                               # server unit tests
cargo clippy --workspace                 # all crates
cd crates/web_mock && cargo run          # frontend verification harness (port 9180)
cd crates/web-app && wasm-pack build --target web --out-dir ../../static/wasm \
  --profile release-slim
```
`release-slim` (defined at the workspace root) is the size-optimized profile
for the wasm build: cargo has no per-package profiles, so a member crate uses
a named workspace profile instead.

## Gotchas
- `ocpp_rs` / `rusqlite` must **not** enter the wasm frontend dependency
  graph. `charge-system-common-model`'s OCPP conversion is behind its `ocpp`
  feature (server-only); the web app depends on it *without* that feature.
- `charge-system-common-model` is consumed by an external repo (the OLED app)
  via git, so its manifest must not use workspace-inherited `version`/`edition`,
  and public API changes are breaking for that repo (e.g. the
  `ChargeProgress::remaining_time: u32` rename of the old `seconds_left: u16`,
  and the removal of `ChargeProgress.energy` — consumers now keep their BMS
  copy's energy from meter values).
- `u64` (energy, battery capacity) crosses wasm-bindgen as JS `BigInt`; keep
  the JS-facing surface on `i64`/`f64`.
- `Cross.toml` configures ARM cross-compilation for the CP host; the wasm
  frontend builds natively (x86_64 → wasm32) — separate build paths.
- Static files are served with `Cache-Control: no-cache` (revalidate every
  load, ETag → 304 when unchanged) — phone browsers otherwise heuristically
  cache `styles.css`/JS and keep serving stale copies across dev iterations.
- Charging-plan notifications are emitted from `evse.rs`
  (`set_charging_plan`) as an `AppliedChargingPlan`: the plan actually applied +
  its computed schedule periods, or `{ Unlimited, [] }` for an explicit
  unlimited apply — a plan whose schedule can't be computed is installed as
  the default (Blocking) plan and notified as such. The web app applies them locally
  (status-tab plan card — with a Schedule row "start → end" where *today* shows
  time only and other days show short weekday (+ day) + time, the end repeating
  a day only when it differs from the start's; a compact stop button in the
  card header — shown for Deadline/Asap plans only, styled like the badge's
  stop-session button — that confirms through the shared dialog and hits
  `/api/plans/stop`; the plan preview shows the same row in place of
  "completes"; BMS target mirror (cleared by plans without a target);
  mirroring plan kind/deadline/power limit into the plan form — the kind
  selector is a chip row: Deadline / ASAP / Blocking / Unlimited, the last two
  hiding all option fields; applying over an active Deadline/Asap plan asks for
  confirmation. The plan tab has no card of its own, the preview is enough
  there). The countdown also runs before the plan starts, showing the
  *charging time* still to come (the whole window before start, then its
  remaining part), not now→schedule-end. A fallback full refresh covers the
  race where the event arrives before the first `/api/state` snapshot.
  Off-peak variants are ignored for now. This extends the OLED unix socket
  contract — **`NoLimit` was renamed `Unlimited` (breaking: the JSON wire
  string changed)** and `Blocking` is now a server-applied state; the OLED
  repo must update its common_model git dep and handle both.
- CP commands (`cmd_tx`) are consumed only inside the dispatcher's OCPP listen
  loop — i.e. **only while the CP websocket is connected**. While the CP is
  disconnected they queue in the mpsc (capacity 8) and flush on reconnect, so
  apply/stop can take effect late (and the web server happily answers 200
  before the command runs).
- The shared notification channel (`main.rs`, capacity 64) fans out to the
  unix-socket notifier, the recording task and one SSE subscriber per
  connection; each subscriber gets its own window, a lagging consumer
  resyncs from the
  REST snapshot.
