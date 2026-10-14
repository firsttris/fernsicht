/**
 * Data access for the client UI. In the desktop app (Tauri) it calls the
 * app's commands: hosts in the LAN, pairing, sessions in the native
 * window, the host on this machine. In a plain browser (development, the
 * demo) it serves demo data.
 */
import {
  demoDevices,
  demoStats,
  demoThisMachine,
  type Device,
  type HostMonitors,
  PAIRING_ERROR_TEXT,
  type SessionStats,
} from "@fernsicht/ui";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { queryOptions } from "@tanstack/react-query";

import type { CodecSetting } from "./bindings/CodecSetting";
import type { Device as AppDevice } from "./bindings/Device";
import type { Failure } from "./bindings/Failure";
import type { FailureCode } from "./bindings/FailureCode";
import type { PairingOpened } from "./bindings/PairingOpened";
import type { SessionState } from "./bindings/SessionState";
import type { Status } from "./bindings/Status";
import type { StreamSettings as AppStreamSettings } from "./bindings/StreamSettings";
import type { ThisMachine as AppMachine } from "./bindings/ThisMachine";

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Whether the UI runs inside the desktop app. */
export const inApp = () => isTauri();

// What the app's commands answer and take, generated from the Rust types
// (apps/desktop/tests/bindings.rs).
export type { Failure } from "./bindings/Failure";
export type { FailureCode } from "./bindings/FailureCode";
export type { HostService } from "./bindings/HostService";
export type { InternetStatus } from "./bindings/InternetStatus";
export type { Rung } from "./bindings/Rung";
export type { SessionState } from "./bindings/SessionState";
/** The host running on this computer, as its control socket reports it. */
export type HostStatus = Status;
/** The video codec choice of the settings page. */
export type VideoCodec = CodecSetting;

export interface ThisMachine extends Partial<AppMachine> {
  /** Account id and code (demo; later the rendezvous server). */
  id?: string;
  code?: string;
}

export const devicesQuery = queryOptions({
  queryKey: ["devices"],
  queryFn: async (): Promise<Device[]> => {
    if (inApp()) return invoke<AppDevice[]>("devices");
    await delay(150);
    return demoDevices;
  },
  // Hosts come and go in the LAN.
  refetchInterval: () => (inApp() ? 5000 : false),
});

export const deviceQuery = (id: string) =>
  queryOptions({
    queryKey: ["devices", id],
    queryFn: async (): Promise<Device> => {
      await delay(50);
      return (
        demoDevices.find((d) => d.id === id) ?? {
          id,
          name: id.replace(/(\d{3})(?=\d)/g, "$1 "),
          online: true,
          os: "",
          gpu: "",
        }
      );
    },
  });

export const thisMachineQuery = queryOptions({
  queryKey: ["this-machine"],
  queryFn: async (): Promise<ThisMachine> =>
    inApp() ? invoke<AppMachine>("this_machine") : demoThisMachine,
  staleTime: () => (inApp() ? 0 : Infinity),
  refetchInterval: () => (inApp() ? 3000 : false),
});

/** The running session with its overlay, refreshed once per second. */
/** The session as the UI shows it: the app's `SessionState`, or the demo's
 * (whose overlay has no glass-to-glass of its own). */
export type Session = Omit<SessionState, "stats"> & {
  stats?: (SessionStats & { monitors?: HostMonitors }) | null;
};

export const sessionQuery = (deviceId: string) =>
  queryOptions({
    queryKey: ["session", deviceId],
    queryFn: async (): Promise<Session> =>
      inApp()
        ? invoke<SessionState>("session")
        : { active: true, deviceId, stats: demoStats(Date.now() / 1000) },
    refetchInterval: 1000,
  });

/** How sessions look (settings page); kept in this browser profile. The
 * mode (gaming or desktop) is chosen when connecting. */
export type StreamSettings = Omit<AppStreamSettings, "gaming">;

export const DEFAULT_SETTINGS: StreamSettings = {
  width: 0,
  height: 0,
  fps: 60,
  bitrateMbit: 0,
  codec: "auto",
  smooth: false,
};
const SETTINGS_KEY = "fernsicht.settings";

export function loadSettings(): StreamSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    return raw
      ? { ...DEFAULT_SETTINGS, ...(JSON.parse(raw) as Partial<StreamSettings>) }
      : DEFAULT_SETTINGS;
  } catch {
    return DEFAULT_SETTINGS;
  }
}

export function saveSettings(s: StreamSettings) {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(s));
  } catch {
    // Storage unavailable: the defaults stay.
  }
}

/** What the app can do (desktop app only). */
export const actions = {
  pair: (address: string, pin: string) => invoke<AppDevice>("pair", { address, pin }),
  connect: (id: string, gaming = false) => {
    const settings: AppStreamSettings = { ...loadSettings(), gaming };
    return invoke<SessionState>("connect", { id, settings });
  },
  forget: (id: string) => invoke<void>("forget", { id }),
  disconnect: async () => {
    if (inApp()) await invoke("disconnect");
  },
  setMuted: async (muted: boolean) => {
    if (inApp()) await invoke("set_muted", { muted });
  },
  /** A key combination for the host (Linux key codes). */
  sendKeys: async (codes: number[]) => {
    if (inApp()) await invoke("send_keys", { codes });
  },
  /** Show the host's monitor `index`. */
  selectMonitor: async (index: number) => {
    if (inApp()) await invoke("select_monitor", { index });
  },
  setMode: async (gaming: boolean) => {
    if (inApp()) await invoke("set_mode", { gaming });
  },
  openPairing: () => invoke<PairingOpened>("open_pairing"),
  share: () => invoke<void>("share_this_machine"),
  setGpuBoost: (on: boolean) => invoke<void>("set_gpu_boost", { on }),
  /** This computer's host's rendezvous service; empty: none. */
  setRendezvous: (url: string) => invoke<void>("set_rendezvous", { url }),
  /** Removes a device paired with this computer's host (by its key); its
   * session ends. */
  unpairFromHost: (device: string) => invoke<void>("unpair_from_host", { device }),
  stopSharing: () => invoke<void>("stop_sharing"),
};

/** Each failure in the UI's words; `null`: the message says it best. */
const FAILURE_TEXT: Record<FailureCode, string | null> = {
  ...PAIRING_ERROR_TEXT,
  "not-paired": "Der Host kennt dieses Gerät nicht mehr. Bitte neu koppeln.",
  unavailable:
    "Der Host konnte die Sitzung nicht starten (kein Bild oder kein Video-Encoder). Sein Log sagt, warum.",
  "no-answer": "Keine Antwort vom Host. Läuft er, und ist die Kopplung offen?",
  "host-silent": "Der Host antwortet nicht. Läuft er noch?",
  unreachable:
    "Der Host ist über das Internet nicht erreichbar. Läuft er, und ist er bei seinem Rendezvous-Dienst angemeldet?",
  "gpu-lost": "Die Grafikkarte dieses Computers reagiert nicht mehr; die Sitzung wurde beendet.",
  cancelled: "Abgebrochen.",
  "pkexec-missing": "Es fehlt pkexec (polkit), um als Administrator einzurichten.",
  other: null,
};

/** Whether `e` is a failure the backend sent (a command's rejection). */
export function isFailure(e: unknown): e is Failure {
  if (typeof e !== "object" || e === null) return false;
  const { code, message } = e as Record<string, unknown>;
  return (
    typeof code === "string" && Object.hasOwn(FAILURE_TEXT, code) && typeof message === "string"
  );
}

/** A failure (or any other error) in the UI's words. */
export function errorText(e: unknown): string {
  if (isFailure(e)) return FAILURE_TEXT[e.code] ?? e.message;
  return e instanceof Error ? e.message : String(e);
}
