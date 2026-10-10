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
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { ArrowRight, Monitor } from "lucide-react";
import { useMemo, useRef, useState } from "react";

import { actions, devicesQuery, errorText, inApp } from "../lib/api";

type Filter = "all" | "online" | "favorites";

export function DevicesPage() {
  const { data: devices, isPending, isError } = useQuery(devicesQuery);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const connectDialog = useRef<HTMLDialogElement>(null);
  const pairDialog = useRef<HTMLDialogElement>(null);
  const [pairAddress, setPairAddress] = useState("");
  const app = inApp();
  const openPairing = (address: string) => {
    setPairAddress(address);
    pairDialog.current?.showModal();
  };

  const visible = useMemo(() => {
    const q = search.trim().toLowerCase();
    const qDigits = q.replace(/\s/g, "");
    return (devices ?? []).filter((d) => {
      if (filter === "online" && !d.online) return false;
      if (filter === "favorites" && !d.favorite) return false;
      if (!q) return true;
      return (
        d.name.toLowerCase().includes(q) ||
        (d.address ?? "").includes(q) ||
        (qDigits !== "" && d.id.includes(qDigits))
      );
    });
  }, [devices, search, filter]);

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h1 className="m-0 text-2xl font-semibold tracking-tight">Geräte</h1>
          <span className="text-muted-foreground">
            {app ? "Rechner in deinem Netzwerk" : "Deine Rechner und freigegebenen Geräte"}
          </span>
        </div>
        <div className="flex gap-2">
          {app ? (
            <Button variant="outline" onClick={() => openPairing("")}>
              Gerät hinzufügen
            </Button>
          ) : (
            // Outside the LAN, pairing needs the rendezvous server.
            <Button variant="outline" disabled title="Pairing kommt mit dem Rendezvous-Server">
              Gerät hinzufügen
            </Button>
          )}
          {!app && (
            <Button onClick={() => connectDialog.current?.showModal()}>
              <ArrowRight size={14} strokeWidth={2.2} />
              Verbinden
            </Button>
          )}
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor="search" className="sr-only">
          Geräte durchsuchen
        </label>
        <Input
          id="search"
          type="search"
          placeholder={app ? "Name oder Adresse suchen …" : "ID oder Name suchen …"}
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

      {isPending && (
        <p className="text-muted-foreground">
          {app ? "Suche Rechner im Netzwerk …" : "Lade Geräte …"}
        </p>
      )}
      {isError && <p className="text-muted-foreground">Geräte konnten nicht geladen werden.</p>}
      {devices && visible.length === 0 && (
        <p className="text-muted-foreground">
          {app && devices.length === 0
            ? "Kein Fernsicht-Host im Netzwerk gefunden. Läuft er auf dem anderen Rechner?"
            : "Keine Geräte gefunden."}
        </p>
      )}

      <div className="grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-4">
        {visible.map((d) => (
          <DeviceCard key={d.id} device={d} onPair={() => openPairing(d.address ?? "")} />
        ))}
      </div>

      <ConnectDialog ref={connectDialog} />
      {app && <PairDialog ref={pairDialog} address={pairAddress} />}
    </>
  );
}

const MODES: { mode: SessionMode; label: string; primary: boolean }[] = [
  { mode: "desktop", label: "Desktop", primary: true },
  { mode: "gaming", label: "Gaming", primary: false },
];

/** "192.168.178.87:47800" without the default port. */
const hostOnly = (address: string) => address.replace(/:47800$/, "");

function DeviceCard({ device: d, onPair }: { device: Device; onPair: () => void }) {
  const app = inApp();
  const unpaired = d.paired === false;
  return (
    <article className="flex flex-col gap-4 rounded-[10px] border border-border p-[18px]">
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-3">
          <div className="flex size-10 items-center justify-center rounded-lg border border-border bg-muted text-subtle-foreground">
            <Monitor size={18} aria-hidden />
          </div>
          <div className="flex flex-col gap-0.5">
            <h2 className="m-0 text-sm font-semibold">{d.name}</h2>
            <span className="font-mono text-xs text-muted-foreground">
              {d.address ? hostOnly(d.address) : formatDeviceId(d.id)}
            </span>
          </div>
        </div>
        <Badge variant={d.online ? "online" : "offline"}>{d.online ? "Online" : "Offline"}</Badge>
      </div>
      <div className="flex flex-wrap gap-1.5">
        {d.os && <Badge>{d.os}</Badge>}
        {d.gpu && <Badge>{d.gpu}</Badge>}
        {unpaired && <Badge>Nicht gekoppelt</Badge>}
        {unpaired && d.pairing && <Badge variant="online">Kopplung offen</Badge>}
        {d.busy && <Badge>In Benutzung</Badge>}
      </div>
      {unpaired ? (
        <Button size="sm" onClick={onPair}>
          Koppeln
        </Button>
      ) : app ? (
        <ConnectButtons device={d} />
      ) : (
        <LinkButtons device={d} />
      )}
    </article>
  );
}

/** Desktop app: start the session in the native window, then show it. */
function ConnectButtons({ device: d }: { device: Device }) {
  const navigate = useNavigate();
  const connect = useMutation({
    mutationFn: ({ mode }: { mode: SessionMode }) =>
      actions.connect(d.id, mode === "gaming").then(() => mode),
    onSuccess: (mode) =>
      navigate({ to: "/session/$deviceId", params: { deviceId: d.id }, search: { mode } }),
  });
  return (
    <div className="flex flex-col gap-2">
      <div className="flex gap-2">
        {MODES.map(({ mode, label, primary }) => (
          <Button
            key={mode}
            size="sm"
            variant={primary ? "default" : "outline"}
            className="flex-1"
            disabled={!d.online || connect.isPending}
            title={d.online ? undefined : `${d.name} ist offline`}
            onClick={() => connect.mutate({ mode })}
          >
            {label}
          </Button>
        ))}
      </div>
      {connect.isError && (
        <p role="alert" className="m-0 text-xs text-muted-foreground">
          {errorText(connect.error)}
        </p>
      )}
      <ForgetButton device={d} />
    </div>
  );
}

function LinkButtons({ device: d }: { device: Device }) {
  return (
    <div className="flex gap-2">
      {MODES.map(({ mode, label, primary }) =>
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
  );
}

/** Forgets a paired host here (it has to be paired again). Asks once. */
function ForgetButton({ device: d }: { device: Device }) {
  const queryClient = useQueryClient();
  const [sure, setSure] = useState(false);
  const forget = useMutation({
    mutationFn: () => actions.forget(d.id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["devices"] }),
  });
  return (
    <div className="flex items-center justify-end gap-2 text-xs">
      {forget.isError && <span role="alert">{errorText(forget.error)}</span>}
      {sure ? (
        <>
          <span className="text-muted-foreground">{d.name} vergessen?</span>
          <Button size="sm" variant="outline" onClick={() => setSure(false)}>
            Nein
          </Button>
          <Button size="sm" variant="destructive" onClick={() => forget.mutate()}>
            Vergessen
          </Button>
        </>
      ) : (
        <button
          type="button"
          className="cursor-pointer border-0 bg-transparent p-0 text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
          onClick={() => setSure(true)}
        >
          Gerät vergessen
        </button>
      )}
    </div>
  );
}

/** Pairing with a host in the LAN: its address and the PIN it shows. */
function PairDialog({ ref, address }: { ref: React.Ref<HTMLDialogElement>; address: string }) {
  const queryClient = useQueryClient();
  const [host, setHost] = useState(address);
  const [pin, setPin] = useState("");
  const [shownFor, setShownFor] = useState(address);
  // A new device to pair with: start over.
  if (shownFor !== address) {
    setShownFor(address);
    setHost(address);
    setPin("");
  }
  const pair = useMutation({
    mutationFn: () => actions.pair(host.trim(), pin),
    onSuccess: async () => {
      setPin("");
      await queryClient.invalidateQueries({ queryKey: ["devices"] });
    },
  });
  const valid = host.trim() !== "" && /^\d{6}$/.test(pin);

  return (
    <dialog
      ref={ref}
      aria-labelledby="pair-title"
      className="m-auto w-full max-w-[400px] rounded-xl border border-border bg-background p-7 text-foreground backdrop:bg-black/60"
      onClose={() => pair.reset()}
    >
      <form
        className="flex flex-col gap-5"
        onSubmit={(e) => {
          e.preventDefault();
          const dialog = e.currentTarget.closest("dialog");
          if (valid) pair.mutate(undefined, { onSuccess: () => dialog?.close() });
        }}
      >
        <div className="flex flex-col gap-1.5">
          <h2 id="pair-title" className="m-0 text-xl font-semibold tracking-tight">
            Gerät koppeln
          </h2>
          <span className="leading-normal text-muted-foreground">
            Auf dem Host unter „Dieser Rechner“ auf „Gerät koppeln“ klicken (oder dort{" "}
            <code>fernsicht-host-agent pair</code> ausführen) und die PIN hier eingeben.
          </span>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="pair-host">Host</Label>
          <Input
            id="pair-host"
            autoComplete="off"
            placeholder="192.168.178.87"
            value={host}
            onChange={(e) => setHost(e.target.value)}
          />
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="pair-pin">PIN</Label>
          <Input
            id="pair-pin"
            inputMode="numeric"
            autoComplete="one-time-code"
            placeholder="000000"
            value={pin}
            onChange={(e) => setPin(e.target.value.replace(/\D/g, "").slice(0, 6))}
            className="h-11 font-mono text-base tracking-[0.3em]"
          />
        </div>
        {pair.isError && (
          <p role="alert" className="m-0 text-muted-foreground">
            {errorText(pair.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="outline"
            onClick={(e) => e.currentTarget.closest("dialog")?.close()}
          >
            Abbrechen
          </Button>
          <Button type="submit" disabled={!valid || pair.isPending}>
            {pair.isPending ? "Kopple …" : "Koppeln"}
          </Button>
        </div>
      </form>
    </dialog>
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
