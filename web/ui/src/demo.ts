/**
 * Demo data until the rendezvous API (phase 3) and the native client
 * bridge (phase 4) exist. Values follow the app mockup.
 */
import type { Device, SessionStats, Stage } from "./types";

export const demoDevices: Device[] = [
  {
    id: "214776390",
    name: "zentrale",
    online: true,
    favorite: true,
    os: "Bazzite · KDE Wayland",
    gpu: "RX 7800 XT · AV1",
  },
  {
    id: "903118452",
    name: "heimserver",
    online: true,
    favorite: false,
    os: "Fedora Server",
    gpu: "Intel iGPU · H.264",
  },
  {
    id: "557204819",
    name: "werkstatt-pc",
    online: false,
    favorite: true,
    os: "Arch · Hyprland",
    gpu: "RTX 3060 · HEVC",
  },
  {
    id: "330691274",
    name: "testrechner",
    online: false,
    favorite: false,
    os: "Omarchy",
    gpu: "Ryzen iGPU · H.264",
  },
];

export const demoThisMachine = { id: "482913057", code: "k7f-2qx" };

const BASE_US: Record<Stage, number> = {
  capture: 1_000,
  encode: 4_000,
  network: 3_000,
  decode: 2_000,
  present: 4_000,
};

/** Plausible, slowly wandering stats for tick `t` (one per second). */
export function demoStats(t: number): SessionStats {
  const wobble = (phase: number, amp: number) => Math.sin(t * 0.7 + phase) * amp;
  return {
    stagesUs: {
      capture: BASE_US.capture + wobble(0, 150),
      encode: BASE_US.encode + wobble(1, 400),
      network: BASE_US.network + wobble(2, 700),
      decode: BASE_US.decode + wobble(3, 200),
      present: BASE_US.present + wobble(4, 300),
    },
    codec: "AV1 · VAAPI",
    fps: 120 + wobble(5, 0.6),
    bitrateBps: 38_000_000 + wobble(6, 2_000_000),
    lossBeforeFec: Math.max(0, 0.004 + wobble(7, 0.003)),
    lossAfterFec: 0,
  };
}
