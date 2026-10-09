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
import { useNavigate, useSearch } from "@tanstack/react-router";
import { ArrowRight, Lock } from "lucide-react";
import { useState } from "react";

const MODES: { value: SessionMode; label: string; hint: string }[] = [
  { value: "desktop", label: "Desktop", hint: "Steuern, Dateien, Zwischenablage" },
  { value: "gaming", label: "Gaming", hint: "Pointer-Lock, Gamepad, Ton" },
];

export function ConnectPage() {
  const search = useSearch({ from: "/" });
  const navigate = useNavigate();
  const [id, setId] = useState(formatDeviceId(search.id ?? ""));
  const [code, setCode] = useState("");
  const [mode, setMode] = useState<SessionMode>("desktop");

  const digits = id.replace(/\D/g, "");
  const codeValid = /^[a-z0-9]{3}-[a-z0-9]{3}$/i.test(code.trim());
  const valid = digits.length === 9 && codeValid;

  return (
    <div className="flex min-h-dvh flex-col bg-background">
      <header className="flex h-14 items-center justify-between border-b border-border px-6">
        <div className="flex items-center gap-2.5">
          <Logo size={26} />
          <span className="font-semibold">Fernsicht</span>
          <Badge>Web</Badge>
        </div>
        <a
          href="#"
          className="text-[13px] text-muted-foreground no-underline hover:text-foreground"
        >
          Anmelden
        </a>
      </header>

      <main className="flex grow items-center justify-center px-4 py-8">
        <form
          className="flex w-full max-w-[400px] flex-col gap-[22px] rounded-xl border border-border p-7"
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
            <h1 className="m-0 text-[22px] font-semibold tracking-tight">
              Mit einem Rechner verbinden
            </h1>
            <p className="m-0 leading-normal text-muted-foreground">
              Direkt im Browser, ohne Installation. Die ID und den Code findest du auf dem
              Zielrechner.
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
                      onChange={() => setMode(m.value)}
                      className="m-0 accent-foreground"
                    />
                    {m.label}
                  </span>
                  <span className="text-xs text-muted-foreground">{m.hint}</span>
                </label>
              ))}
            </div>
          </fieldset>

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
      </main>
    </div>
  );
}
