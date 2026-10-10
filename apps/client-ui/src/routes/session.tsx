import { Button, SessionView } from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { AppWindow } from "lucide-react";

import { actions, deviceQuery, errorText, inApp, sessionQuery } from "../lib/api";

/**
 * Running session. In the desktop app the video itself is drawn by the
 * native winit/Vulkan window (no webview in the image path); this view is
 * its control surface and the same layout the web viewer uses.
 */
export function SessionPage() {
  const { deviceId } = useParams({ from: "/session/$deviceId" });
  const { mode } = useSearch({ from: "/session/$deviceId" });
  const navigate = useNavigate();
  const app = inApp();
  const { data: device } = useQuery({ ...deviceQuery(deviceId), enabled: !app });
  const { data: session } = useQuery(sessionQuery(deviceId));
  const name = session?.deviceName ?? device?.name ?? "…";
  const back = () => void navigate({ to: "/devices" });

  if (app && session && !session.active) {
    return (
      <div className="flex h-dvh flex-col items-center justify-center gap-4 bg-background p-6 text-center">
        <h1 className="m-0 text-xl font-semibold tracking-tight">Sitzung mit {name} beendet</h1>
        {session.error && (
          <p role="alert" className="m-0 max-w-[480px] text-muted-foreground">
            {errorText(session.error)}
          </p>
        )}
        <Button onClick={back}>Zurück zu den Geräten</Button>
      </div>
    );
  }

  return (
    <SessionView
      session={{
        deviceName: name,
        width: 2560,
        height: 1440,
        path: "P2P",
        encrypted: true,
      }}
      stats={session?.stats ?? undefined}
      mode={mode}
      onModeChange={(m) => {
        void actions.setMode(m === "gaming");
        void navigate({ to: ".", search: { mode: m }, replace: true });
      }}
      onMuteChange={(muted) => void actions.setMuted(muted)}
      onSendKeys={(codes) => void actions.sendKeys(codes)}
      onDisconnect={() => void actions.disconnect().then(back)}
    >
      {app ? <NativeWindowNotice name={name} /> : undefined}
    </SessionView>
  );
}

function NativeWindowNotice({ name }: { name: string }) {
  return (
    <div className="flex max-w-[520px] flex-col items-center gap-3 p-6 text-center text-muted-foreground">
      <AppWindow size={40} strokeWidth={1.5} aria-hidden />
      <span className="text-foreground">Das Bild von „{name}“ läuft in einem eigenen Fenster.</span>
      <span>
        Dort gehen Maus und Tastatur an den Host, im Vollbild auch Windows-Taste und Alt+Tab.
        Strg+Alt+Shift+F schaltet Vollbild um, Strg+Alt+Shift+Q beendet die Sitzung. Strg+Alt+Entf
        und Co. gibt es oben unter „Tasten senden“.
      </span>
    </div>
  );
}
