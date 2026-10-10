import { SessionView, demoDevices, demoStats, formatDeviceId } from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { Navigate, useNavigate, useParams, useSearch } from "@tanstack/react-router";

import { currentSession } from "../lib/host";
import { LiveSessionPage } from "./live";

/**
 * Browser session: live with the host that served the page (WebRTC), or
 * the demo. A reload loses the live session (the PIN was used once), so
 * it goes back to the connect page.
 */
export function ViewerSessionPage() {
  const { deviceId } = useParams({ from: "/session/$deviceId" });
  const { mode } = useSearch({ from: "/session/$deviceId" });
  const navigate = useNavigate();
  const live = currentSession();
  const setMode = (m: typeof mode) =>
    void navigate({ to: ".", search: { mode: m }, replace: true });

  if (live) {
    return (
      <LiveSessionPage
        session={live}
        mode={mode}
        onModeChange={setMode}
        onEnd={() => void navigate({ to: "/" })}
      />
    );
  }
  if (deviceId === "host") return <Navigate to="/" />;
  return <DemoSession deviceId={deviceId} mode={mode} onModeChange={setMode} />;
}

function DemoSession({
  deviceId,
  mode,
  onModeChange,
}: {
  deviceId: string;
  mode: "desktop" | "gaming";
  onModeChange: (m: "desktop" | "gaming") => void;
}) {
  const navigate = useNavigate();
  const { data: stats } = useQuery({
    queryKey: ["stats", deviceId],
    queryFn: () => demoStats(Date.now() / 1000),
    refetchInterval: 1000,
  });
  const name = demoDevices.find((d) => d.id === deviceId)?.name ?? formatDeviceId(deviceId);

  return (
    <SessionView
      session={{ deviceName: name, width: 2560, height: 1440, path: "P2P", encrypted: true }}
      stats={stats}
      mode={mode}
      onModeChange={onModeChange}
      onDisconnect={() => void navigate({ to: "/", search: { id: deviceId } })}
    />
  );
}
