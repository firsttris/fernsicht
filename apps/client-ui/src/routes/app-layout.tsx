import { Button, Logo, formatDeviceId } from "@fernsicht/ui";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Outlet } from "@tanstack/react-router";
import { Check, Clock, Copy, Monitor, Settings, Shield } from "lucide-react";
import { useEffect, useState } from "react";

import { type HostStatus, actions, errorText, thisMachineQuery } from "../lib/api";

const NAV = [
  { to: "/devices", label: "Geräte", icon: Monitor },
  { to: "/history", label: "Verlauf", icon: Clock },
  { to: "/access", label: "Zugriffe & Rechte", icon: Shield },
  { to: "/settings", label: "Einstellungen", icon: Settings },
] as const;

export function AppLayout() {
  return (
    <div className="flex min-h-dvh flex-wrap bg-background">
      <nav
        aria-label="Hauptnavigation"
        className="flex max-w-[260px] min-w-[220px] flex-[1_1_240px] flex-col gap-1 border-r border-border px-3 py-4 max-md:max-w-none max-md:border-r-0 max-md:border-b"
      >
        <div className="flex items-center gap-2.5 px-2 pt-2 pb-5">
          <Logo />
          <span className="text-[15px] font-semibold tracking-tight">Fernsicht</span>
        </div>
        {NAV.map(({ to, label, icon: Icon }) => (
          <Link
            key={to}
            to={to}
            className="flex items-center gap-2.5 rounded-md px-2.5 py-2 text-muted-foreground transition-colors hover:text-foreground"
            activeProps={{ className: "bg-secondary font-medium !text-foreground" }}
          >
            <Icon size={16} aria-hidden />
            {label}
          </Link>
        ))}
        <div className="grow" />
        <ThisMachineCard />
      </nav>
      <main className="flex min-w-0 flex-[999_1_560px] flex-col gap-6 px-8 py-7 max-md:px-4">
        <Outlet />
      </main>
    </div>
  );
}

function ThisMachineCard() {
  const { data } = useQuery(thisMachineQuery);
  const [copied, setCopied] = useState(false);
  if (!data) return null;
  if (data.name !== undefined) return <LocalHostCard name={data.name} host={data.host ?? null} />;
  if (!data.id) return null;
  const id = data.id;

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(`${formatDeviceId(id)} · ${data.code}`);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard can be unavailable (insecure context); nothing to do.
    }
  };

  return (
    <div className="mt-4 flex flex-col gap-2 rounded-lg border border-border p-3.5">
      <span className="text-xs text-muted-foreground">Dieser Rechner</span>
      <span className="font-mono text-lg font-medium tracking-wider">{formatDeviceId(id)}</span>
      <div className="flex items-center justify-between">
        <span className="font-mono text-muted-foreground">Code: {data.code}</span>
        <Button
          variant="outline"
          size="icon"
          className="size-7"
          aria-label={copied ? "Kopiert" : "ID und Code kopieren"}
          onClick={copy}
        >
          {copied ? <Check size={14} /> : <Copy size={14} />}
        </Button>
      </div>
    </div>
  );
}

/** "482913" → "482 913". */
const formatPin = (pin: string) => pin.replace(/(\d{3})(?=\d)/, "$1 ");

/** "4:59" */
const formatLeft = (s: number) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;

/**
 * Desktop app: this computer and the host running on it. "Gerät koppeln"
 * opens pairing there and shows the PIN to type on the other device.
 */
function LocalHostCard({ name, host }: { name: string; host: HostStatus | null }) {
  const queryClient = useQueryClient();
  const [pin, setPin] = useState<{ pin: string; until: number } | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const open = useMutation({
    mutationFn: actions.openPairing,
    onSuccess: (r) => {
      setPin({ pin: r.pin, until: Date.now() + r.expires_in_s * 1000 });
      void queryClient.invalidateQueries({ queryKey: ["this-machine"] });
    },
  });
  // The PIN counts down; it is gone once the host closed pairing (a
  // device paired, or time ran out).
  useEffect(() => {
    if (!pin) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [pin]);
  const left = pin ? Math.max(0, Math.round((pin.until - now) / 1000)) : 0;
  const showPin = pin && left > 0 && host?.pairing != null;

  return (
    <div className="mt-4 flex flex-col gap-2 rounded-lg border border-border p-3.5">
      <span className="text-xs text-muted-foreground">Dieser Rechner</span>
      <span className="text-base font-medium">{name}</span>
      {host ? (
        <>
          <span className="text-xs text-muted-foreground">
            Host aktiv · {host.paired.length}{" "}
            {host.paired.length === 1 ? "Gerät gekoppelt" : "Geräte gekoppelt"}
            {host.session && ` · verbunden mit ${host.session.client}`}
          </span>
          {showPin ? (
            <div className="flex flex-col gap-1" aria-live="polite">
              <span className="text-xs text-muted-foreground">PIN für das neue Gerät</span>
              <span className="font-mono text-2xl font-medium tracking-wider">
                {formatPin(pin.pin)}
              </span>
              <span className="text-xs text-muted-foreground">gilt noch {formatLeft(left)}</span>
            </div>
          ) : (
            <Button
              size="sm"
              variant="outline"
              disabled={open.isPending}
              onClick={() => open.mutate()}
            >
              Gerät koppeln
            </Button>
          )}
          {open.isError && (
            <span role="alert" className="text-xs text-muted-foreground">
              {errorText(open.error)}
            </span>
          )}
        </>
      ) : (
        <span className="text-xs text-muted-foreground">
          Kein Host aktiv. Damit andere Geräte auf diesen Rechner zugreifen können, den Host
          einrichten (docs/install.md).
        </span>
      )}
    </div>
  );
}
