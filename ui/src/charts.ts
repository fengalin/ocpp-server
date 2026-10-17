import Chart from "chart.js/auto";
import { state } from "./state";
import type { MeterValues } from "./state";

const COLORS: Record<string, string> = {
  power: "#4cc38a",
  energy: "#5aa9e6",
  voltage: "#e6b45a",
  temp: "#e0705a",
};

export interface MetricDef {
  key: keyof ClientMetrics;
  label: string;
  unit: string;
  get: (mv: MeterValues) => number | null;
}

type ClientMetrics = { power: boolean; energy: boolean; voltage: boolean; temp: boolean };

export const METRICS: MetricDef[] = [
  { key: "power", label: "Power", unit: "kW", get: (mv) => (mv.power != null ? mv.power / 1000 : null) },
  { key: "energy", label: "Energy", unit: "kWh", get: (mv) => mv.energy / 1000 },
  { key: "voltage", label: "Voltage", unit: "V", get: (mv) => mv.voltage },
  { key: "temp", label: "Temperature", unit: "°C", get: (mv) => mv.temperature },
];

const charts = new Map<string, Chart>();

export function downsample(meter_values: MeterValues[], max: number): MeterValues[] {
  if (meter_values.length <= max) return meter_values;
  const bucket = Math.ceil(meter_values.length / max);
  const out: MeterValues[] = [];

  for (let i = 0; i < meter_values.length; i += bucket) {
    const part = meter_values.slice(i, i + bucket);
    const last = part[part.length - 1];
    let psum = 0;
    let pn = 0;
    let vsum = 0;
    let vn = 0;
    let tsum = 0;
    let tn = 0;
    for (const mv of part) {
      if (mv.power != null) {
        psum += s.power;
        pn++;
      }
      if (mv.voltage != null) {
        vsum += mv.voltage;
        vn++;
      }
      if (mv.temperature != null) {
        tsum += mv.temperature;
        tn++;
      }
    }
    out.push({
      ts: last.ts,
      energy: last.energy,
      power: pn ? psum / pn : null,
      voltage: vn ? vsum / vn : null,
      temperature: tn ? tsum / tn : null,
    });
  }
  return out;
}

function tickFmt(windowMs: number) {
  return (v: unknown) => {
    const d = new Date(Number(v));
    if (windowMs >= 6 * 3_600_000) {
      return (
        d.toLocaleDateString([], { weekday: "short" }) +
        " " +
        d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
      );
    }
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  };
}

function ensureCanvas(key: string, label: string): HTMLCanvasElement {
  let wrap = document.getElementById("chart-" + key);
  if (!wrap) {
    const host = document.getElementById("charts")!;
    wrap = document.createElement("div");
    wrap.className = "chart-wrap";
    wrap.id = "chart-" + key;
    const h = document.createElement("h3");
    h.textContent = label;
    const box = document.createElement("div");
    box.className = "chart-box";
    const canvas = document.createElement("canvas");
    box.appendChild(canvas);
    wrap.appendChild(h);
    wrap.appendChild(box);
    host.appendChild(wrap);
  }
  return wrap.querySelector("canvas")!;
}

// (Re)create or update one chart per enabled metric from state.meter_values.
export function syncCharts(): void {
  const active = METRICS.filter((m) => state.metrics[m.key]);
  for (const [k, c] of Array.from(charts)) {
    if (!active.some((m) => m.key === k)) {
      c.destroy();
      charts.delete(k);
      document.getElementById("chart-" + k)?.remove();
    }
  }
  if (!state.meter_values.length) return;
  const ds = downsample(state.meter_values, 700);

  for (const m of active) {
    const data = ds.map((mv) => ({ x: Date.parse(mv.ts), y: m.get(mv) }));
    const existing = charts.get(m.key);
    if (existing) {
      (existing.data.datasets[0] as { data: unknown }).data = data;
      existing.update("none");
      continue;
    }
    charts.set(
      m.key,
      new Chart(ensureCanvas(m.key, `${m.label} (${m.unit})`), {
        type: "line",
        data: {
          datasets: [
            {
              label: `${m.label} (${m.unit})`,
              data,
              borderColor: COLORS[m.key],
              backgroundColor: COLORS[m.key] + "22",
              borderWidth: 1.5,
              pointRadius: 0,
              tension: 0.2,
              fill: true,
              spanGaps: false,
            },
          ],
        },
        options: {
          responsive: true,
          maintainAspectRatio: false,
          animation: false,
          parsing: false,
          interaction: { mode: "nearest", axis: "x", intersect: false },
          scales: {
            x: {
              type: "linear",
              ticks: {
                callback: tickFmt(state.window),
                maxTicksLimit: 6,
              },
            },
            y: {
              beginAtZero: m.key === "power",
              suggestedMax: m.key === "power" ? 8 : undefined,
            },
          },
        },
      }),
    );
  }
}
