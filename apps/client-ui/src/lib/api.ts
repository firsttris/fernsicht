/**
 * Data access for the client UI. Today it serves demo data; in phase 4 the
 * same functions call Tauri commands (local host service, native video
 * window) and the rendezvous API (devices, pairing).
 */
import {
  type Device,
  type SessionStats,
  demoDevices,
  demoStats,
  demoThisMachine,
} from "@fernsicht/ui";
import { queryOptions } from "@tanstack/react-query";

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

export const devicesQuery = queryOptions({
  queryKey: ["devices"],
  queryFn: async (): Promise<Device[]> => {
    await delay(150);
    return demoDevices;
  },
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
  queryFn: async () => demoThisMachine,
  staleTime: Infinity,
});

/** Overlay stats of the running session, refreshed once per second. */
export const sessionStatsQuery = (deviceId: string) =>
  queryOptions({
    queryKey: ["session-stats", deviceId],
    queryFn: async (): Promise<SessionStats> => demoStats(Date.now() / 1000),
    refetchInterval: 1000,
  });
