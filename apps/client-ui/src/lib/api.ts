/**
 * Data access for the client UI. In the desktop app (Tauri) it calls the
 * app's commands: hosts in the LAN, pairing, sessions in the native
 * window, the host on this machine. In a plain browser (development, the
 * demo) it serves demo data.
 */
import {
  type Device,
  type SessionStats,
  demoDevices,
  demoStats,
  demoThisMachine,
} from "@fernsicht/ui";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { queryOptions } from "@tanstack/react-query";

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Whether the UI runs inside the desktop app. */
export const inApp = () => isTauri();

/** The host running on this computer, as its control socket reports it. */
export interface HostStatus {
  name: string;
  key: string;
  paired: { name: string; key: string }[];
  session: { client: string; address: string } | null;
  /** Seconds pairing stays open, if it is. */
  pairing: number | null;
  /** The GPU is clocked up during sessions. */
  gpu_boost?: boolean;
}

/** The host service on this computer (desktop app). */
export interface HostService {
  installed: boolean;
  active: boolean;
  /** Installed host's version. */
  version: string | null;
  /** Version of the host in this AppImage; null outside one. */
  bundled: string | null;
  /** This app can set up the host (it runs from the AppImage). */
  canInstall: boolean;
}

export interface ThisMachine {
  /** Account id and code (demo; later the rendezvous server). */
  id?: string;
  code?: string;
  /** Desktop app: the computer's name and its host, if one runs. */
  name?: string;
  host?: HostStatus | null;
  service?: HostService;
}

/** The session the app runs in the native window. */
export interface SessionState {
  active: boolean;
  deviceId?: string | null;
  deviceName?: string | null;
  stats?: SessionStats | null;
  /** Why it ended, if it failed. */
  error?: string | null;
}

export const devicesQuery = queryOptions({
  queryKey: ["devices"],
  queryFn: async (): Promise<Device[]> => {
    if (inApp()) return invoke<Device[]>("devices");
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
          favorite: false,
          os: "",
          gpu: "",
        }
      );
    },
  });

export const thisMachineQuery = queryOptions({
  queryKey: ["this-machine"],
  queryFn: async (): Promise<ThisMachine> =>
    inApp() ? invoke<ThisMachine>("this_machine") : demoThisMachine,
  staleTime: () => (inApp() ? 0 : Infinity),
  refetchInterval: () => (inApp() ? 3000 : false),
});

/** The running session with its overlay, refreshed once per second. */
export const sessionQuery = (deviceId: string) =>
  queryOptions({
    queryKey: ["session", deviceId],
    queryFn: async (): Promise<SessionState> =>
      inApp()
        ? invoke<SessionState>("session")
        : { active: true, deviceId, stats: demoStats(Date.now() / 1000) },
    refetchInterval: 1000,
  });

/** How sessions look (settings page); kept in this browser profile. */
export interface StreamSettings {
  /** Stream size; 0 × 0 = the host's screen. */
  width: number;
  height: number;
  fps: number;
  /** 0 = the host's choice for the size. */
  bitrateMbit: number;
  /** "auto": HEVC where both sides can, else H.264. */
  codec: VideoCodec;
}

export type VideoCodec = "auto" | "h264" | "hevc";

export const DEFAULT_SETTINGS: StreamSettings = {
  width: 0,
  height: 0,
  fps: 60,
  bitrateMbit: 0,
  codec: "auto",
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
  pair: (address: string, pin: string) => invoke<Device>("pair", { address, pin }),
  connect: (id: string, gaming = false) =>
    invoke<SessionState>("connect", { id, settings: { ...loadSettings(), gaming } }),
  forget: (id: string) => invoke<void>("forget", { id }),
  disconnect: async () => {
    if (inApp()) await invoke("disconnect");
  },
  setMuted: async (muted: boolean) => {
    if (inApp()) await invoke("set_muted", { muted });
  },
  setMode: async (gaming: boolean) => {
    if (inApp()) await invoke("set_mode", { gaming });
  },
  openPairing: () => invoke<{ pin: string; expires_in_s: number }>("open_pairing"),
  share: () => invoke<void>("share_this_machine"),
  setGpuBoost: (on: boolean) => invoke<void>("set_gpu_boost", { on }),
  stopSharing: () => invoke<void>("stop_sharing"),
};

/** The backend's (English) errors in the UI's words. */
export function errorText(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e);
  const known: [string, string][] = [
    ["wrong PIN", "Falsche PIN."],
    ["too many wrong PINs", "Zu viele falsche PINs. Kopplung am Host neu öffnen."],
    ["not in pairing mode", "Am Host ist keine Kopplung offen."],
    ["no answer from", "Keine Antwort vom Host. Läuft er, und ist die Kopplung offen?"],
    ["not paired with the host", "Der Host kennt dieses Gerät nicht mehr. Bitte neu koppeln."],
    ["no packets from", "Der Host antwortet nicht. Läuft er noch?"],
    ["cancelled", "Abgebrochen."],
    ["pkexec is missing", "Es fehlt pkexec (polkit), um als Administrator einzurichten."],
  ];
  return known.find(([needle]) => raw.includes(needle))?.[1] ?? raw;
}
