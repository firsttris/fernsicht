import {
  Badge,
  Button,
  Input,
  Label,
  Logo,
  type SessionMode,
  cn,
  formatDeviceId,
} from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { ArrowRight, Lock } from "lucide-react";
import { type ReactNode, useState } from "react";

import { ConnectError, type HostInfo, connect, connectErrorText, fetchHostInfo } from "../lib/host";

const MODES: { value: SessionMode; label: string; hint: string }[] = [
  { value: "desktop", label: "Desktop", hint: "Steuern, Dateien, Zwischenablage" },
  { value: "gaming", label: "Gaming", hint: "Pointer-Lock, Gamepad, Ton" },
];

/** The host serving this page, if any (otherwise the demo). */
export const hostInfoQuery = {
  queryKey: ["host-info"],
  queryFn: fetchHostInfo,
  staleTime: Infinity,
  retry: false,
} as const;

export function ConnectPage() {
  const { data: host, isPending } = useQuery(hostInfoQuery);
  if (isPending) return null;
  return <Page>{host ? <HostForm host={host} /> : <DemoForm />}</Page>;
}

function Page({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-dvh flex-col bg-background">
      <header className="flex h-14 items-center justify-between border-b border-border px-6">
        <div className="flex items-center gap-2.5">
          <Logo size={26} />
          <span className="font-semibold">Fernsicht</span>
          <Badge>Web</Badge>
        </div>
      </header>
      <main className="flex grow items-center justify-center px-4 py-8">{children}</main>
    </div>
  );
}

function ModeChoice({ mode, onChange }: { mode: SessionMode; onChange: (m: SessionMode) => void }) {
  return (
    <fieldset className="m-0 flex flex-col gap-2 border-0 p-0">
      <legend className="mb-2 p-0 text-[13px] font-medium">Modus</legend>
      <div className="grid grid-cols-2 gap-2">
        {MODES.map((m) => (
          <label
            key={m.value}
            className={cn(
              "flex cursor-pointer flex-col gap-1 rounded-lg border p-3 transition-colors",
              mode === m.value ? "border-foreground" : "border-border hover:border-ring",
            )}
          >
            <span className="flex items-center gap-2 font-medium">
              <input
                type="radio"
                name="mode"
                value={m.value}
                checked={mode === m.value}
                onChange={() => onChange(m.value)}
                className="m-0 accent-foreground"
              />
              {m.label}
            </span>
            <span className="text-xs text-muted-foreground">{m.hint}</span>
          </label>
        ))}
      </div>
    </fieldset>
  );
}

const formClass =
  "flex w-full max-w-[400px] flex-col gap-[22px] rounded-xl border border-border p-7";

/** Served by a host: its PIN starts a WebRTC session. */
function HostForm({ host }: { host: HostInfo }) {
  const navigate = useNavigate();
  const [pin, setPin] = useState("");
  const [mode, setMode] = useState<SessionMode>("desktop");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const valid = /^\d{6}$/.test(pin);

  return (
    <form
      aria-labelledby="connect-title"
      className={formClass}
      onSubmit={(e) => {
        e.preventDefault();
        if (!valid || busy) return;
        setBusy(true);
        setError(null);
        connect(pin, host.name)
          .then(() =>
            navigate({ to: "/session/$deviceId", params: { deviceId: "host" }, search: { mode } }),
          )
          .catch((err: unknown) => {
            setError(connectErrorText(err instanceof ConnectError ? err.code : "failed"));
            setPin("");
            setBusy(false);
          });
      }}
    >
      <div className="flex flex-col gap-1.5">
        <h1 id="connect-title" className="m-0 text-[22px] font-semibold tracking-tight">
          Mit {host.name} verbinden
        </h1>
        <p className="m-0 leading-normal text-muted-foreground">
          Am Host in der Fernsicht-App unter „Dieser Rechner“ auf „Gerät koppeln“ klicken (oder dort{" "}
          <code>fernsicht-host-agent pair</code> ausführen) und die PIN hier eingeben.
        </p>
      </div>

      <div className="flex flex-col gap-2">
        <Label htmlFor="pin">PIN</Label>
        <Input
          id="pin"
          inputMode="numeric"
          autoComplete="one-time-code"
          placeholder="000000"
          value={pin}
          onChange={(e) => setPin(e.target.value.replace(/\D/g, "").slice(0, 6))}
          className="h-11 font-mono text-base tracking-[0.3em]"
        />
      </div>

      <ModeChoice mode={mode} onChange={setMode} />

      {error && (
        <p role="alert" className="m-0 leading-normal text-muted-foreground">
          {error}
        </p>
      )}

      <Button type="submit" size="lg" disabled={!valid || busy}>
        {busy ? "Verbinde …" : "Verbinden"}
        <ArrowRight size={14} strokeWidth={2.2} />
      </Button>

      <div className="flex items-start gap-2.5 rounded-lg border border-border bg-muted p-3 text-xs leading-normal text-muted-foreground">
        <Lock size={16} className="mt-px shrink-0 text-subtle-foreground" aria-hidden />
        <span>
          Bild, Ton und Eingaben laufen verschlüsselt (WebRTC). Die PIN gilt einmal. Für die
          niedrigste Latenz nutze die Desktop-App.
        </span>
      </div>
    </form>
  );
}

/** Without a host (demo, later the rendezvous server): device id and code. */
function DemoForm() {
  const search = useSearch({ from: "/" });
  const navigate = useNavigate();
  const [id, setId] = useState(formatDeviceId(search.id ?? ""));
  const [code, setCode] = useState("");
  const [mode, setMode] = useState<SessionMode>("desktop");

  const digits = id.replace(/\D/g, "");
  const codeValid = /^[a-z0-9]{3}-[a-z0-9]{3}$/i.test(code.trim());
  const valid = digits.length === 9 && codeValid;

  return (
    <form
      aria-labelledby="connect-title"
      className={formClass}
      onSubmit={(e) => {
        e.preventDefault();
        if (!valid) return;
        // Phase 5: the one-time code goes to the rendezvous server, which
        // asks the host to confirm and then sets up WebRTC (str0m).
        void navigate({
          to: "/session/$deviceId",
          params: { deviceId: digits },
          search: { mode },
        });
      }}
    >
      <div className="flex flex-col gap-1.5">
        <h1 id="connect-title" className="m-0 text-[22px] font-semibold tracking-tight">
          Mit einem Rechner verbinden
        </h1>
        <p className="m-0 leading-normal text-muted-foreground">
          Direkt im Browser, ohne Installation. Die ID und den Code findest du auf dem Zielrechner.
        </p>
      </div>

      <div className="flex flex-col gap-2">
        <Label htmlFor="rid">Geräte-ID</Label>
        <Input
          id="rid"
          inputMode="numeric"
          autoComplete="off"
          placeholder="000 000 000"
          value={id}
          onChange={(e) => setId(formatDeviceId(e.target.value).slice(0, 11))}
          className="h-11 font-mono text-base tracking-wider"
        />
      </div>

      <div className="flex flex-col gap-2">
        <Label htmlFor="code">Einmal-Code</Label>
        <Input
          id="code"
          autoComplete="one-time-code"
          placeholder="xxx-xxx"
          value={code}
          onChange={(e) => setCode(e.target.value.toLowerCase().slice(0, 7))}
          className="h-11 font-mono text-base tracking-wider"
        />
      </div>

      <ModeChoice mode={mode} onChange={setMode} />

      <Button type="submit" size="lg" disabled={!valid}>
        Verbinden
        <ArrowRight size={14} strokeWidth={2.2} />
      </Button>

      <div className="flex items-start gap-2.5 rounded-lg border border-border bg-muted p-3 text-xs leading-normal text-muted-foreground">
        <Lock size={16} className="mt-px shrink-0 text-subtle-foreground" aria-hidden />
        <span>
          Ende-zu-Ende verschlüsselt. Der Zielrechner muss die Verbindung bestätigen. Für die
          niedrigste Latenz nutze die Desktop-App.
        </span>
      </div>
    </form>
  );
}
