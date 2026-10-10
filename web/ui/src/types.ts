/**
 * Shapes shared by the client UI, the web viewer and (later) the Rust API.
 * Once the axum API exists these are generated from Rust via ts-rs/specta.
 */

export type SessionMode = "desktop" | "gaming";

export interface Device {
  id: string;
  name: string;
  online: boolean;
  favorite: boolean;
  os: string;
  /** GPU and best available encoder, e.g. "RX 7800 XT · AV1". */
  gpu: string;
  /**
   * Hosts in the LAN (desktop app): paired with this machine. Devices
   * from the account (demo, later the rendezvous server) leave it out.
   */
  paired?: boolean;
  /** Pairing is open on the host: it shows a PIN. */
  pairing?: boolean;
  /** Someone is connected to it. */
  busy?: boolean;
  /** Where it answered, e.g. "192.168.178.87:47800". */
  address?: string | null;
}

/** Mirrors `fernsicht_core::latency::Stage`, in pipeline order. */
export type Stage = "capture" | "encode" | "network" | "decode" | "present";

export const STAGES: { key: Stage; label: string; color: string }[] = [
  { key: "capture", label: "Capture", color: "bg-stage-capture" },
  { key: "encode", label: "Encode", color: "bg-stage-encode" },
  { key: "network", label: "Netz", color: "bg-stage-network" },
  { key: "decode", label: "Decode", color: "bg-stage-decode" },
  { key: "present", label: "Anzeige", color: "bg-stage-present" },
];

/** Mirrors `fernsicht_render::overlay::StreamInfo` plus stage averages. */
export interface SessionStats {
  /** Average duration per stage, microseconds. */
  stagesUs: Record<Stage, number>;
  codec: string;
  fps: number;
  bitrateBps: number;
  /** Packet loss on the wire before FEC, 0–1. */
  lossBeforeFec: number;
  /** Frames lost after FEC, 0–1. */
  lossAfterFec: number;
}

export interface SessionInfo {
  deviceName: string;
  width: number;
  height: number;
  /** "P2P" or "Relay". */
  path: "P2P" | "Relay";
  encrypted: boolean;
}

/** The host's monitors, as the host reports them. */
export interface HostMonitors {
  /** Index of the one shown. */
  current: number;
  list: { name: string; width: number; height: number }[];
}
