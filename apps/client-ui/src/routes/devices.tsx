import {
  Badge,
  Button,
  type Device,
  Input,
  Label,
  Segmented,
  type SessionMode,
  formatDeviceId,
} from "@fernsicht/ui";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { ArrowRight, Monitor } from "lucide-react";
import { useMemo, useRef, useState } from "react";

import { devicesQuery } from "../lib/api";

type Filter = "all" | "online" | "favorites";

export function DevicesPage() {
  const { data: devices, isPending, isError } = useQuery(devicesQuery);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const connectDialog = useRef<HTMLDialogElement>(null);

  const visible = useMemo(() => {
    const q = search.trim().toLowerCase();
    const qDigits = q.replace(/\s/g, "");
    return (devices ?? []).filter((d) => {
      if (filter === "online" && !d.online) return false;
      if (filter === "favorites" && !d.favorite) return false;
      if (!q) return true;
      return d.name.toLowerCase().includes(q) || (qDigits !== "" && d.id.includes(qDigits));
    });
  }, [devices, search, filter]);

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h1 className="m-0 text-2xl font-semibold tracking-tight">Geräte</h1>
          <span className="text-muted-foreground">Deine Rechner und freigegebenen Geräte</span>
        </div>
        <div className="flex gap-2">
          {/* Pairing needs the rendezvous server (phase 3). */}
          <Button variant="outline" disabled title="Pairing kommt mit dem Rendezvous-Server">
            Gerät hinzufügen
          </Button>
          <Button onClick={() => connectDialog.current?.showModal()}>
            <ArrowRight size={14} strokeWidth={2.2} />
            Verbinden
          </Button>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor="search" className="sr-only">
          Geräte durchsuchen
        </label>
        <Input
          id="search"
          type="search"
          placeholder="ID oder Name suchen …"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="max-w-[360px] flex-[1_1_280px]"
        />
        <Segmented<Filter>
          label="Filter"
          value={filter}
          onChange={setFilter}
          options={[
            { value: "all", label: "Alle" },
            { value: "online", label: "Online" },
            { value: "favorites", label: "Favoriten" },
          ]}
        />
      </div>

      {isPending && <p className="text-muted-foreground">Lade Geräte …</p>}
      {isError && <p className="text-muted-foreground">Geräte konnten nicht geladen werden.</p>}
      {devices && visible.length === 0 && (
        <p className="text-muted-foreground">Keine Geräte gefunden.</p>
      )}

      <div className="grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-4">
        {visible.map((d) => (
          <DeviceCard key={d.id} device={d} />
        ))}
      </div>

      <ConnectDialog ref={connectDialog} />
    </>
  );
}

function DeviceCard({ device: d }: { device: Device }) {
  const modes: { mode: SessionMode; label: string; primary: boolean }[] = [
    { mode: "desktop", label: "Desktop", primary: true },
    { mode: "gaming", label: "Gaming", primary: false },
  ];
  return (
    <article className="flex flex-col gap-4 rounded-[10px] border border-border p-[18px]">
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-3">
          <div className="flex size-10 items-center justify-center rounded-lg border border-border bg-muted text-subtle-foreground">
            <Monitor size={18} aria-hidden />
          </div>
          <div className="flex flex-col gap-0.5">
            <h2 className="m-0 text-sm font-semibold">{d.name}</h2>
            <span className="font-mono text-xs text-muted-foreground">{formatDeviceId(d.id)}</span>
          </div>
        </div>
        <Badge variant={d.online ? "online" : "offline"}>{d.online ? "Online" : "Offline"}</Badge>
      </div>
      <div className="flex flex-wrap gap-1.5">
        {d.os && <Badge>{d.os}</Badge>}
        {d.gpu && <Badge>{d.gpu}</Badge>}
      </div>
      <div className="flex gap-2">
        {modes.map(({ mode, label, primary }) =>
          d.online ? (
            <Button
              key={mode}
              asChild
              size="sm"
              variant={primary ? "default" : "outline"}
              className="flex-1"
            >
              <Link to="/session/$deviceId" params={{ deviceId: d.id }} search={{ mode }}>
                {label}
              </Link>
            </Button>
          ) : (
            <Button
              key={mode}
              size="sm"
              variant={primary ? "default" : "outline"}
              className="flex-1"
              disabled
              title={`${d.name} ist offline`}
            >
              {label}
            </Button>
          ),
        )}
      </div>
    </article>
  );
}

function ConnectDialog({ ref }: { ref: React.Ref<HTMLDialogElement> }) {
  const navigate = useNavigate();
  const [id, setId] = useState("");
  const digits = id.replace(/\D/g, "");
  const valid = digits.length === 9;

  return (
    <dialog
      ref={ref}
      aria-labelledby="connect-title"
      className="m-auto w-full max-w-[400px] rounded-xl border border-border bg-background p-7 text-foreground backdrop:bg-black/60"
    >
      <form
        method="dialog"
        className="flex flex-col gap-5"
        onSubmit={(e) => {
          if (!valid) {
            e.preventDefault();
            return;
          }
          void navigate({
            to: "/session/$deviceId",
            params: { deviceId: digits },
            search: { mode: "desktop" },
          });
        }}
      >
        <div className="flex flex-col gap-1.5">
          <h2 id="connect-title" className="m-0 text-xl font-semibold tracking-tight">
            Mit einem Rechner verbinden
          </h2>
          <span className="leading-normal text-muted-foreground">
            Die ID steht auf dem Zielrechner unter „Dieser Rechner“.
          </span>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="connect-id">Geräte-ID</Label>
          <Input
            id="connect-id"
            inputMode="numeric"
            autoComplete="off"
            placeholder="000 000 000"
            value={id}
            onChange={(e) => setId(formatDeviceId(e.target.value).slice(0, 11))}
            className="h-11 font-mono text-base tracking-wider"
          />
        </div>
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="outline"
            onClick={(e) => e.currentTarget.closest("dialog")?.close()}
          >
            Abbrechen
          </Button>
          <Button type="submit" disabled={!valid}>
            Verbinden
          </Button>
        </div>
      </form>
    </dialog>
  );
}
