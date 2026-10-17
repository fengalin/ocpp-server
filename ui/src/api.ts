import { state } from "./state";
import type {
  EventLogEntry,
  Plan,
  PlanPreview,
  MeterValues,
  ServerState,
  Session,
  Status,
} from "./state";

async function get<T>(url: string): Promise<T> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return (await res.json()) as T;
}

async function post<T>(url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return (await res.json()) as T;
}

export const api = {
  getState: () => get<ServerState>("/api/state"),
  getMeterValues: (from: Date, to: Date) =>
    get<MeterValues[]>(
      `/api/meter_values?from=${encodeURIComponent(from.toISOString())}&to=${encodeURIComponent(to.toISOString())}`,
    ),
  getEvents: () => get<EventLogEntry[]>("/api/events"),
  previewPlan: (body: object) => post<PlanPreview>("/api/plans/preview", body),
  createPlan: (body: object) => post<Plan>("/api/plans", body),
  stopPlan: () => post<unknown>("/api/plans/stop"),
  stopSession: () => post<unknown>("/api/session/stop"),
  setMeterValueInterval: (interval: number) =>
    post<unknown>("/api/cp/set_meter_value_interval", { interval }),
  rebootCp: () => post<unknown>("/api/cp/reboot"),
};

export interface StreamHandlers {
  meter_values: (meter_values: MeterValues[], session: Session | null, soc: number | null) => void;
  status: (status: Status, session: Session | null) => void;
  plan: () => void;
}

// EventSource reconnects automatically; on every (re)open the caller does a
// full catch-up (state + visible window + events) so no data is lost.
export function connectStream(onOpen: () => void, h: StreamHandlers): EventSource {
  const es = new EventSource("/api/stream");
  es.onopen = () => {
    state.connected = true;
    onOpen();
  };
  es.onerror = () => {
    state.connected = false;
  };
  es.addEventListener("meter_values", (e) => {
    const d = JSON.parse((e as MessageEvent).data);
    h.meter_values([d.meter_values], d.session, d.soc ?? null);
  });
  es.addEventListener("status", (e) => {
    const d = JSON.parse((e as MessageEvent).data);
    h.status(d.status, d.session);
  });
  es.addEventListener("plan", () => h.plan());
  es.addEventListener("resync", onOpen);
  return es;
}
