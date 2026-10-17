import { api } from "./api";
import { state } from "./state";

function $(id: string): HTMLElement {
  return document.getElementById(id)!;
}

export function initActions(onChange: () => void): void {
  const actMsg = $("act-msg");

  async function run(fn: () => Promise<unknown>, okMsg: string): Promise<void> {
    try {
      await fn();
      actMsg.textContent = okMsg;
      actMsg.className = "msg ok";
    } catch (e) {
      actMsg.textContent = String(e);
      actMsg.className = "msg err";
    }
    onChange();
  }

  ($("act-stop-session") as HTMLButtonElement).addEventListener("click", () => {
    if (confirm("Stop the current charging session?")) {
      void run(() => api.stopSession(), "Session stopped.");
    }
  });
  ($("act-stop-plan") as HTMLButtonElement).addEventListener("click", () => {
    if (confirm("Stop the active plan? A running session keeps going unmanaged.")) {
      void run(() => api.stopPlan(), "Plan stopped.");
    }
  });
  ($("act-reboot") as HTMLButtonElement).addEventListener("click", () => {
    if (confirm("Reboot the charge point?")) {
      void run(() => api.rebootCp(), "Reboot issued.");
    }
  });
  ($("act-set-meter-value-interval") as HTMLButtonElement).addEventListener("click", () => {
    void run(() => api.setMeterValueInterval(150), "Set MeterValue interval issued.");
  });
  ($("act-reset-meter-value-interval") as HTMLButtonElement).addEventListener("click", () => {
    void run(() => api.setMeterValueInterval(4126), "Reset MeterValue interval issued.");
  });
}
