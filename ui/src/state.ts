// Client-side types mirror src/model.rs on the server.

export type Status =
  | "Available"
  | "Preparing"
  | "Charging"
  | "SuspendedByEVSE"
  | "Finished"
  | "Rebooting"
  | "Faulted";

export interface MeterValues {
  ts: string;
  energy: number; // Wh (cumulative grid import)
  power: number | null; // W (grid power), null when not charging
  voltage: number; // V
  temperature: number; // °C
}

export interface Session {
  id: string;
  start_ts: string;
  status: Status;
  initial_energy: number; // Wh
  initial_soc: number;
  cur_soc: number;
}

export interface ChargingPeriod {
  start_at: string;
  duration: number; // seconds
  power_limit: number; // W
}

export interface PlanPreview {
  energy: number; // Wh
  soc_delta: number;
  soc_result: number;
  duration: number; // seconds
  completes_at: string;
  feasible: boolean;
  avg_battery: number; // W (average power received by the battery)
  periods: ChargingPeriod[];
  note: string | null;
}

export interface Plan {
  id: string;
  kind: "Deadline" | "Asap";
  start_at: string;
  target_soc: number;
  deadline: string | null;
  initial_soc: number;
  periods: ChargingPeriod[];
  state: "Active" | "Completed" | "Cancelled";
  preview: PlanPreview;
  created_ts: string;
}

export interface LivePlan {
  plan: Plan;
  remaining_energy: number; // Wh
  remaining_time: number; // seconds
  eta: string;
}

export interface Characteristics {
  battery_capacity: number; // Wh
  max_power: number; // W
  static_loss: number; // W
}

export interface ServerState {
  ts: string;
  characteristics: Characteristics;
  status: Status;
  soc: number;
  session: Session | null;
  active_plan: LivePlan | null;
}

export interface EventLogEntry {
  ts: string;
  status: Status;
  note: string | null;
}

export interface ClientState {
  server: ServerState | null;
  meter_values: MeterValues[]; // visible metrics window
  events: EventLogEntry[]; // newest first
  connected: boolean;
  window: number; // metrics window in ms
  metrics: { power: boolean; energy: boolean; voltage: boolean; temp: boolean };
}

export const state: ClientState = {
  server: null,
  meter_values: [],
  events: [],
  connected: false,
  window: 3_600_000,
  metrics: { power: true, energy: true, voltage: false, temp: false },
};

export function fmtTime(iso: string, withDate: boolean): string {
  const d = new Date(iso);
  const t = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return withDate
    ? `${d.toLocaleDateString([], { month: "short", day: "numeric" })} ${t}`
    : t;
}

export function fmtDur(secs: number): string {
  const m = Math.round(secs / 60);
  const h = Math.floor(m / 60);
  return h > 0
    ? `${h} h ${String(m % 60).padStart(2, "0")} min`
    : `${m} min`;
}
