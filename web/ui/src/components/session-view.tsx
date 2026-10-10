import {
  ClipboardList,
  Monitor,
  MonitorPlay,
  Settings2,
  Upload,
  Volume2,
  VolumeX,
} from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";

import { cn } from "../lib/utils";
import type { SessionInfo, SessionMode, SessionStats } from "../types";
import { Button } from "./button";
import { LatencyOverlay } from "./latency-overlay";

export interface SessionViewProps {
  session: SessionInfo;
  stats: SessionStats | undefined;
  mode: SessionMode;
  onModeChange: (mode: SessionMode) => void;
  /** The sound button was pressed (muted = true). */
  onMuteChange?: (muted: boolean) => void;
  onDisconnect: () => void;
  /** The video surface. Defaults to a placeholder frame. */
  children?: ReactNode;
}

/**
 * Running session: video, floating toolbar, latency overlay.
 * Strg+Alt+F toggles toolbar and overlay.
 */
export function SessionView({
  session,
  stats,
  mode,
  onModeChange,
  onMuteChange,
  onDisconnect,
  children,
}: SessionViewProps) {
  const [chromeVisible, setChromeVisible] = useState(true);
  const [muted, setMuted] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.altKey && e.key.toLowerCase() === "f") {
        e.preventDefault();
        setChromeVisible((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="relative h-dvh min-h-[480px] overflow-hidden bg-black text-foreground">
      <div className="absolute inset-0 flex items-center justify-center bg-[#111113]">
        {children ?? <VideoPlaceholder session={session} />}
      </div>

      {chromeVisible && (
        <>
          {/* Toolbar and overlay share one column so the overlay always sits
              below the toolbar, however often the toolbar wraps (phones). */}
          <div className="pointer-events-none absolute inset-x-4 top-3.5 flex flex-col gap-3">
            <div
              role="toolbar"
              aria-label="Session"
              className="pointer-events-auto flex max-w-full flex-wrap items-center justify-center gap-1 self-center rounded-xl border border-border bg-overlay p-1.5 backdrop-blur"
            >
              <div className="mr-1 flex items-center gap-2 border-r border-border pr-2.5 pl-1.5">
                <span className="size-2 rounded-full bg-success" aria-hidden />
                <span className="font-semibold">{session.deviceName}</span>
                <span className="text-xs text-muted-foreground">
                  {session.path}
                  {session.encrypted && " · E2E"}
                </span>
              </div>
              {(["desktop", "gaming"] as const).map((m) => (
                <Button
                  key={m}
                  size="sm"
                  variant={mode === m ? "secondary" : "ghost"}
                  aria-pressed={mode === m}
                  onClick={() => onModeChange(m)}
                  className={cn(mode !== m && "text-muted-foreground")}
                >
                  {m === "desktop" ? "Desktop" : "Gaming"}
                </Button>
              ))}
              <Divider />
              <Button size="icon" variant="ghost" aria-label="Bildschirm wählen">
                <Monitor size={16} />
              </Button>
              <Button size="icon" variant="ghost" aria-label="Zwischenablage">
                <ClipboardList size={16} />
              </Button>
              <Button size="icon" variant="ghost" aria-label="Dateien senden">
                <Upload size={16} />
              </Button>
              <Button
                size="icon"
                variant="ghost"
                aria-label={muted ? "Ton einschalten" : "Ton ausschalten"}
                aria-pressed={muted}
                onClick={() => {
                  onMuteChange?.(!muted);
                  setMuted(!muted);
                }}
              >
                {muted ? <VolumeX size={16} /> : <Volume2 size={16} />}
              </Button>
              <Button size="icon" variant="ghost" aria-label="Einstellungen">
                <Settings2 size={16} />
              </Button>
              <Divider />
              <Button size="sm" variant="destructive" onClick={onDisconnect}>
                Trennen
              </Button>
            </div>

            {stats && <LatencyOverlay stats={stats} className="pointer-events-auto self-end" />}
          </div>

          <div className="absolute bottom-4 left-4 hidden items-center gap-2 sm:flex rounded-lg border border-border bg-overlay px-2.5 py-1.5 text-xs text-muted-foreground">
            <Kbd>Strg</Kbd>
            <Kbd>Alt</Kbd>
            <Kbd>F</Kbd>
            Leiste ein- und ausblenden
          </div>
        </>
      )}
    </div>
  );
}

function Divider() {
  return <span className="mx-1 h-5 w-px bg-border" aria-hidden />;
}

function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="rounded border border-ring/60 px-1.5 py-px font-mono text-foreground">
      {children}
    </kbd>
  );
}

function VideoPlaceholder({ session }: { session: SessionInfo }) {
  return (
    <div className="flex aspect-video w-[78%] max-w-[980px] flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-[#3f3f46] text-muted-foreground">
      <MonitorPlay size={40} strokeWidth={1.5} aria-hidden />
      <span>
        Videostream von „{session.deviceName}“ – {session.width} × {session.height}
      </span>
    </div>
  );
}
