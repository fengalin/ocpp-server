import { api } from "./api";
import { state, fmtDur, fmtTime } from "./state";
import type { PlanPreview } from "./state";

let timer: number | undefined;
let reqId = 0;
let onChange: () => void = () => {};

function $(id: string): HTMLElement {
  return document.getElementById(id)!;
}

function setMsg(text: string, ok: boolean): void {
  const el = $("plan-msg");
  el.textContent = text;
  el.className = "msg" + (text ? (ok ? " ok" : " err") : "");
}

function buildBody(): Record<string, unknown> | null {
  const deadline = $("deadline") as HTMLInputElement;
  const target = $("target") as HTMLInputElement;
  const isDeadline = ($("mode-deadline") as HTMLInputElement).checked;
  const body: Record<string, unknown> = {
    kind: isDeadline ? "Deadline" : "Asap",
    target_soc: Number(target.value),
    deadline: null,
  };
  if (isDeadline) {
    if (!deadline.value) return null;
    body.deadline = new Date(deadline.value).toISOString();
  }
  return body;
}

function renderPreview(p: PlanPreview | null): void {
  const el = $("preview-panel");
  if (!p) {
    el.classList.add("hidden");
    return;
  }
  const loss = state.server?.characteristics.static_loss ?? 400;
  const rows: [string, string][] = [
    ["Energy to add", `${(p.energy / 1000).toFixed(1)} kWh`],
    ["SoC", `+${p.soc_delta.toFixed(1)} % → ${p.soc_result.toFixed(1)} %`],
    ["Duration", fmtDur(p.duration)],
    ["Completes", fmtTime(p.completes_at, true)],
    ["Avg battery power", `${(p.avg_battery / 1000).toFixed(3)} kW`],
  ];
  const periods = p.periods
    .filter((x) => x.power_limit > 0)
    .map((x) => {
      const s = new Date(x.start_at);
      const eIso = new Date(s.getTime() + x.duration * 1000).toISOString();
      return `<div class="period"><span>${fmtTime(x.start_at, true)} – ${fmtTime(eIso, true)}</span><b>@ ${(x.power_limit / 1000).toFixed(3)} kW limit ≈ ${((x.power_limit - loss) / 1000).toFixed(3)} kW to battery</b></div>`;
    })
    .join("");
  el.classList.remove("hidden");
  el.innerHTML =
    `<h2>Preview</h2>` +
    (p.feasible ? "" : `<div class="warn">${p.note ?? "Not feasible"}</div>`) +
    `<div class="kv-grid">${rows
      .map(([l, v]) => `<div class="kv"><span>${l}</span><b>${v}</b></div>`)
      .join("")}</div>` +
    (periods ? `<h3>Periods</h3>${periods}` : "");
}

async function doPreview(): Promise<void> {
  const body = buildBody();
  if (!body) {
    setMsg("Pick a deadline.", false);
    return;
  }
  const id = ++reqId;
  try {
    const p = await api.previewPlan(body);
    if (id !== reqId) return;
    renderPreview(p);
    setMsg("", true);
  } catch (e) {
    if (id !== reqId) return;
    renderPreview(null);
    setMsg(String(e), false);
  }
}

function schedulePreview(): void {
  if (timer) window.clearTimeout(timer);
  timer = window.setTimeout(doPreview, 400);
}

export function initPlan(cb: () => void): void {
  onChange = cb;
  const target = $("target") as HTMLInputElement;
  const deadline = $("deadline") as HTMLInputElement;
  const targetVal = $("target-val");
  const applyBtn = $("plan-apply") as HTMLButtonElement;
  const modeD = $("mode-deadline") as HTMLInputElement;
  const modeA = $("mode-asap") as HTMLInputElement;

  const sync = () => {
    targetVal.textContent = `${Math.round(Number(target.value))} %`;
    $("deadline-wrap").classList.toggle("hidden", modeA.checked);
    schedulePreview();
  };
  modeD.addEventListener("change", sync);
  modeA.addEventListener("change", sync);
  target.addEventListener("input", sync);
  deadline.addEventListener("input", () => {
    onChange();
    schedulePreview();
  });

  applyBtn.addEventListener("click", async () => {
    const body = buildBody();
    if (!body) {
      setMsg("Pick a deadline.", false);
      return;
    }
    applyBtn.disabled = true;
    const id = ++reqId;
    try {
      await api.createPlan(body);
      if (id === reqId) setMsg("Plan applied.", true);
    } catch (e) {
      if (id === reqId) setMsg(String(e), false);
    } finally {
      applyBtn.disabled = false;
    }
    onChange();
  });
}

// FIXME what's that?
// Deadline input is compared against sim time on the server; pre-fill it
// relative to sim "now" so the user has a sane default.
export function ensureDeadlineDefault(): void {
  const deadline = $("deadline") as HTMLInputElement;
  if (deadline.value || !state.server) return;
  const d = new Date(new Date(state.server.ts).getTime() + 4 * 3_600_000);
  const p = (n: number) => String(n).padStart(2, "0");
  deadline.value = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}
