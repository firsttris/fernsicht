import { SessionView } from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";

import { deviceQuery, sessionStatsQuery } from "../lib/api";

/**
 * Running session. In the desktop app the video itself is drawn by the
 * native winit/Vulkan window (no webview in the image path); this view is
 * its control surface and the same layout the web viewer uses.
 */
export function SessionPage() {
  const { deviceId } = useParams({ from: "/session/$deviceId" });
  const { mode } = useSearch({ from: "/session/$deviceId" });
  const navigate = useNavigate();
  const { data: device } = useQuery(deviceQuery(deviceId));
  const { data: stats } = useQuery(sessionStatsQuery(deviceId));

  return (
    <SessionView
      session={{
        deviceName: device?.name ?? "…",
        width: 2560,
        height: 1440,
        path: "P2P",
        encrypted: true,
      }}
      stats={stats}
      mode={mode}
      onModeChange={(m) => void navigate({ to: ".", search: { mode: m }, replace: true })}
      onDisconnect={() => void navigate({ to: "/devices" })}
    />
  );
}
