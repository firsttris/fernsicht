import {
  ClipboardList,
  Keyboard,
  MousePointer2,
  ZoomOut,
  Maximize,
  Minimize,
  Monitor,
  MonitorPlay,
  Settings2,
  Upload,
  Volume2,
  VolumeX,
} from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";

import { cn } from "../lib/utils";
import type { HostMonitors, SessionInfo, SessionMode, SessionStats } from "../types";
import { Button } from "./button";
import { LatencyOverlay } from "./latency-overlay";
import { MenuButton } from "./menu-button";
import { SendKeysMenu } from "./send-keys";

export interface SessionViewProps {
  session: SessionInfo;
  stats: SessionStats | undefined;
  mode: SessionMode;
  onModeChange: (mode: SessionMode) => void;
  /** The sound button was pressed (muted = true). */
  onMuteChange?: (muted: boolean) => void;
  /** A key combination from the "send keys" menu (Linux key codes). */
  onSendKeys?: (codes: number[]) => void;
  /** The host's monitors; with two or more, a menu switches between them. */
  monitors?: HostMonitors;
  onSelectMonitor?: (index: number) => void;
  /** Opens the on-screen keyboard (the web viewer on phones). */
  onShowKeyboard?: () => void;
  /** Touch screens: the finger clicks where it is, or works as a touchpad. */
  touchMode?: "direct" | "trackpad";
  onTouchModeChange?: (mode: "direct" | "trackpad") => void;
  /** The picture is zoomed in here (pinch); offers a way back. */
  zoomed?: boolean;
  onResetZoom?: () => void;
  /** Fullscreen button (the web viewer; the app's window has its own). */
  fullscreen?: boolean;
  onFullscreenChange?: (on: boolean) => void;
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
  onSendKeys,
  monitors,
  onSelectMonitor,
  onShowKeyboard,
  touchMode,
  onTouchModeChange,
  zoomed = false,
  onResetZoom,
  fullscreen = false,
  onFullscreenChange,
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
              {onShowKeyboard && (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label="Bildschirmtastatur"
                  onClick={onShowKeyboard}
                >
                  <Keyboard size={16} />
                </Button>
              )}
              {onSendKeys && <SendKeysMenu onSend={onSendKeys} />}
              {touchMode && onTouchModeChange && (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label="Touchpad-Modus"
                  aria-pressed={touchMode === "trackpad"}
                  onClick={() =>
                    onTouchModeChange(touchMode === "trackpad" ? "direct" : "trackpad")
                  }
                >
                  <MousePointer2 size={16} />
                </Button>
              )}
              {zoomed && onResetZoom && (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label="Zoom zurücksetzen"
                  onClick={onResetZoom}
                >
                  <ZoomOut size={16} />
                </Button>
              )}
              {monitors && monitors.list.length > 1 && onSelectMonitor && (
                <MenuButton
                  label="Bildschirm wählen"
                  icon={<Monitor size={16} />}
                  items={monitors.list.map((m, i) => ({
                    label: `Bildschirm ${i + 1} · ${m.name} · ${m.width}×${m.height}`,
                    checked: i === monitors.current,
                    onSelect: () => onSelectMonitor(i),
                  }))}
                />
              )}
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
              {onFullscreenChange && (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label={fullscreen ? "Vollbild verlassen" : "Vollbild"}
                  aria-pressed={fullscreen}
                  onClick={() => onFullscreenChange(!fullscreen)}
                >
                  {fullscreen ? <Minimize size={16} /> : <Maximize size={16} />}
                </Button>
              )}
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
