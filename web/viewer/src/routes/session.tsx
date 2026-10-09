import { SessionView, demoDevices, demoStats, formatDeviceId } from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";

/**
 * Browser session. Phase 5 replaces the placeholder with a <video> fed by
 * WebRTC (str0m on the host), plus the Pointer Lock and Gamepad APIs in
 * gaming mode; the stats then come from getStats().
 */
export function ViewerSessionPage() {
  const { deviceId } = useParams({ from: "/session/$deviceId" });
  const { mode } = useSearch({ from: "/session/$deviceId" });
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
      onModeChange={(m) => void navigate({ to: ".", search: { mode: m }, replace: true })}
      onDisconnect={() => void navigate({ to: "/", search: { id: deviceId } })}
    />
  );
}
