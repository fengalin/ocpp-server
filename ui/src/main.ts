import { api, connectStream } from "./api";
import { state, fmtDur, fmtTime } from "./state";
import type { LivePlan, MeterValues, Session, Status } from "./state";
import { METRICS, syncCharts } from "./charts";
import { ensureDeadlineDefault, initPlan } from "./plan";
import { initActions } from "./actions";

const STATUS_COLORS: Record<Status, string> = {
  Available: "#7d8590",
  Preparing: "#5aa9e6",
  Charging: "#4cc38a",
  SuspendedByEVSE: "#e6b45a",
  Finished: "#9b8ae6",
  Rebooting: "#e08a5a",
  Faulted: "#e05a5a",
};

let chartDirty = false;
let catchUpTimer: number | undefined;
let catchUpAt = 0; // earliest time the next scheduled catch-up may fire

function $(id: string): HTMLElement {
  return document.getElementById(id)!;
}

function lastMeterValues(): MeterValues | undefined {
  return state.meter_values[state.meter_values.length - 1];
}

function planCardHtml(lp: LivePlan, soc: number): string {
  return (
    `<div class="kv"><span>Target</span><b>${lp.plan.target_soc.toFixed(0)} %</b></div>` +
    `<div class="kv"><span>Remaining</span><b>${Math.max(0, lp.plan.target_soc - soc).toFixed(1)} % · ${Math.max(0, lp.remaining_energy / 1000).toFixed(1)} kWh</b></div>` +
    `<div class="kv"><span>Remaining time</span><b>${fmtDur(lp.remaining_time)}</b></div>` +
    `<div class="kv"><span>ETA</span><b>${fmtTime(lp.eta, true)}</b></div>`
  );
}

function renderHeader(): void {
  const s = state.server;
  $("clock").textContent = s
    ? `${fmtTime(s.ts, true)}`
    : "connecting…";
  $("conn").className = "conn" + (state.connected ? " ok" : " bad");
}

function renderStatus(): void {
  const s = state.server;
  if (!s) return;
  const badge = $("status-badge");
  badge.textContent = s.status;
  const c = STATUS_COLORS[s.status];
  badge.style.color = c;
  badge.style.borderColor = c;
  badge.style.background = c + "1f";
  badge.style.cursor = s.status === "Charging" && s.session ? "pointer" : "default";
  badge.title = s.status === "Charging" && s.session ? "Click to stop session" : "";
  const hasSession = s.session !== null;
  badge.classList.toggle("clickable", s.status === "Charging" && hasSession);

  $("soc-text").textContent = `${s.soc.toFixed(1)} %`;
  $("soc-fill").style.width = `${Math.min(100, Math.max(0, s.soc))}%`;
  const mark = $("soc-target-mark");
  if (s.active_plan) {
    mark.style.left = `${s.active_plan.plan.target_soc}%`;
    mark.classList.remove("hidden");
    $("soc-target").textContent = `target ${s.active_plan.plan.target_soc.toFixed(0)} %`;
  } else {
    mark.classList.add("hidden");
    $("soc-target").textContent = "";
  }

  const last = lastMeterValues();
  const charging = s.status === "Charging" && last?.power != null;
  const sessionEnergy = s.session
    ? (last?.energy ?? 0) - s.session.initial_energy
    : null;
  const tiles: [string, string][] = [
    ["Power", charging ? `${(last.power! / 1000).toFixed(3)} kW` : "—"],
    ["Session energy", sessionEnergy != null ? `${(sessionEnergy / 1000).toFixed(1)} kWh` : "—"],
    ["Voltage", last?.voltage ? `${last.voltage} V` : "—"],
    ["CP temperature", last?.temperature ? `${last.temperature} °C` : "—"],
  ];
  $("tiles").innerHTML = tiles
    .map(([l, v]) => `<div class="tile"><div class="tile-v">${v}</div><div class="tile-l">${l}</div></div>`)
    .join("");

  const stopVisible = s.status === "Charging" && s.session;
  $("stop-session-btn").classList.toggle("hidden", !stopVisible);

  const sc = $("session-card");
  if (s.session) {
    sc.classList.remove("hidden");
    sc.innerHTML =
      `<h2>Charging session</h2>` +
      `<div class="kv"><span>Started</span><b>${fmtTime(s.session.start_ts, true)}</b></div>` +
      `<div class="kv"><span>Initial SoC</span><b>${s.session.initial_soc.toFixed(1)} %</b></div>` +
      ``;
  } else {
    sc.classList.add("hidden");
  }

  const pc = $("plan-card");
  const pa = $("plan-active");
  if (s.active_plan) {
    pc.classList.remove("hidden");
    pc.innerHTML = `<h2>Active plan</h2>${planCardHtml(s.active_plan, s.soc)}`;
    pa.classList.remove("hidden");
    pa.innerHTML = `<h2>Active plan</h2>${planCardHtml(s.active_plan, s.soc)}`;
  } else {
    pc.classList.add("hidden");
    pa.classList.add("hidden");
  }

  // FIXME a note could be useful
  // $("note").innerHTML = `EV ${s.sim.ev_plugged ? "plugged" : "unplugged"}`;
}

function renderLog(): void {
  $("log-list").innerHTML = state.events.length
    ? state.events
        .map(
          (e) =>
            `<div class="log-entry"><span class="log-time">${fmtTime(e.ts, true)}</span>` +
            `<span class="status-chip" style="color:${STATUS_COLORS[e.status]};border-color:${STATUS_COLORS[e.status]}55">${e.status}</span>` +
            (e.note ? `<span class="log-note">${e.note}</span>` : "") +
            `</div>`,
        )
        .join("")
    : "<p class='muted'>No events yet.</p>";
}

function render(): void {
  renderHeader();
  renderStatus();
  renderLog();
  ensureDeadlineDefault();
  if ($("tab-metrics").classList.contains("active")) syncCharts();
}

async function catchUp(): Promise<void> {
  try {
    const s = await api.getState();
    state.server = s;
    const now = new Date(s.ts);
    state.meter_values = await api.getMeterValues(
      new Date(now.getTime() - state.window),
      now,
    );
    const ev = await api.getEvents();
    state.events = ev.slice().reverse();
    render();
  } catch (e) {
    console.error("catch-up failed", e);
  }
}

// Throttled catch-up: at most one per 1500 ms, but never starves under a
// continuous Meter Value stream. A plain debounce would reset its timer on every
// event and, never fire again after the initial load, freezing the status tab and
// letting state.meter_values grow without bound.
function scheduleCatchUp(): void {
  if (catchUpTimer !== undefined) return;
  const delay = Math.max(0, catchUpAt - Date.now());
  catchUpTimer = window.setTimeout(() => {
    catchUpTimer = undefined;
    catchUpAt = Date.now() + 1500;
    void catchUp();
  }, delay);
}

function onMeterValues(meter_values: MeterValues[], soc: number | null): void {
  if (!state.server) return;
  if (soc != null) {
    // Mirrors the server: while a session is active, soc == cur_soc.
    state.server.soc = soc;
  }
  if (meter_values.length) {
    state.meter_values.push(...meter_values);
    const from =
      new Date(state.server.ts).getTime() - state.window;
    state.meter_values = state.meter_values.filter((mv) => Date.parse(mv.ts) >= from);
    chartDirty = true;
  }
  renderStatus();
  scheduleCatchUp();
}

function onStatus(status: Status, session: Session | null): void {
  if (!state.server) return;
  state.server.status = status;
  if (session) state.server.session = session;
  scheduleCatchUp();
  render();
}

function initTabs(): void {
  const btns = Array.from(document.querySelectorAll<HTMLButtonElement>(".tab"));
  btns.forEach((b) =>
    b.addEventListener("click", () => {
      btns.forEach((x) => x.classList.toggle("active", x === b));
      document
        .querySelectorAll(".tabpane")
        .forEach((p) => p.classList.toggle("active", p.id === "tab-" + b.dataset.tab));
      if (b.dataset.tab === "metrics") syncCharts();
    }),
  );
}

function initMetricsControls(): void {
  const scales: [string, number][] = [
    ["10 min", 600_000],
    ["1 h", 3_600_000],
    ["6 h", 21_600_000],
    ["24 h", 86_400_000],
    ["7 d", 604_800_000],
  ];
  const seg = $("scale-btns");
  seg.innerHTML = scales
    .map(
      ([l, ms]) =>
        `<button class="btn${ms === state.window ? " active" : ""}" data-ms="${ms}">${l}</button>`,
    )
    .join("");
  seg.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
    b.addEventListener("click", () => {
      state.window = Number(b.dataset.ms);
      seg
        .querySelectorAll<HTMLButtonElement>("button")
        .forEach((x) => x.classList.toggle("active", x === b));
      void catchUp();
    }),
  );

  const tseg = $("metric-toggles");
  tseg.innerHTML = METRICS.map(
    (m) =>
      `<button class="btn${state.metrics[m.key] ? " active" : ""}" data-m="${m.key}">${m.label}</button>`,
  ).join("");
  tseg.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
    b.addEventListener("click", () => {
      const k = b.dataset.m! as keyof typeof state.metrics;
      state.metrics[k] = !state.metrics[k];
      b.classList.toggle("active", state.metrics[k]);
      syncCharts();
    }),
  );
}

function metricsTabActive(): boolean {
  return $("tab-metrics").classList.contains("active");
}

initTabs();
initMetricsControls();
initPlan(() => {
  void catchUp();
});
initActions(() => {
  void catchUp();
});


// Click-to-stop on badge: attach once on init, remove once confirmed.
{
  const badge = $("status-badge");
  const overlay = $("confirm-overlay");
  const cancelBtn = $("confirm-cancel") as HTMLButtonElement;
  let resolve: ((ok: boolean) => void) | null = null;
  let confirm_open = false;

  function showConfirm(): Promise<boolean> {
    confirm_open = true;
    return new Promise((res) => {
      resolve = res;
      $("confirm-msg").textContent = "Stop the current charging session?";
      overlay.classList.remove("hidden");
      cancelBtn.focus();
    });
  }
  function hideConfirm(ok: boolean): void {
    overlay.classList.add("hidden");
    confirm_open = false;
    if (resolve) {
      resolve(ok);
      resolve = null;
    }
  }
  cancelBtn.addEventListener("click", () => hideConfirm(false));
  $("confirm-ok").addEventListener("click", () => hideConfirm(true));
  // Clicking the backdrop (not the dialog box) cancels.
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) hideConfirm(false);
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && confirm_open) hideConfirm(false);
  });

  async function onStopClick(): Promise<void> {
    const s = state.server;
    if (confirm_open) return; // dialog already open
    if (s?.status !== "Charging" || !s.session) return;
    const ok = await showConfirm();
    if (ok) {
      void api.stopSession().catch(console.error);
    }
  }
  badge.addEventListener("click", onStopClick);
  $("stop-session-btn").addEventListener("click", onStopClick);
}

connectStream(() => {
  void catchUp();
}, {
  meter_values: onMeterValues,
  status: onStatus,
  plan: () => {
    void catchUp();
  },
});

void catchUp();

setInterval(() => {
  renderHeader();
  if (chartDirty && metricsTabActive()) {
    chartDirty = false;
    syncCharts();
  }
}, 1000);
