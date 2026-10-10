import { Segmented } from "@fernsicht/ui";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import {
  DEFAULT_SETTINGS,
  type StreamSettings,
  actions,
  errorText,
  inApp,
  loadSettings,
  saveSettings,
  thisMachineQuery,
} from "../lib/api";

const SIZES = [
  { value: "host", label: "Wie der Host", size: [0, 0] },
  { value: "2560", label: "1440p", size: [2560, 1440] },
  { value: "1920", label: "1080p", size: [1920, 1080] },
  { value: "1280", label: "720p", size: [1280, 720] },
] as const;
type SizeKey = (typeof SIZES)[number]["value"];

const FPS = ["30", "60", "120", "144"] as const;
const BITRATES = [
  { value: "0", label: "Automatisch" },
  { value: "10", label: "10 Mbit/s" },
  { value: "20", label: "20 Mbit/s" },
  { value: "35", label: "35 Mbit/s" },
  { value: "50", label: "50 Mbit/s" },
  { value: "80", label: "80 Mbit/s" },
] as const;

const sizeKey = (s: StreamSettings): SizeKey =>
  SIZES.find((x) => x.size[0] === s.width && x.size[1] === s.height)?.value ?? "host";

/** How sessions look. Applies to the next session. */
export function SettingsPage() {
  const [s, setS] = useState(loadSettings);
  const update = (next: StreamSettings) => {
    setS(next);
    saveSettings(next);
  };

  return (
    <>
      <div className="flex flex-col gap-1">
        <h1 className="m-0 text-2xl font-semibold tracking-tight">Einstellungen</h1>
        <span className="text-muted-foreground">Gilt für die nächste Sitzung.</span>
      </div>

      <section className="flex max-w-[640px] flex-col gap-6">
        <Setting
          title="Auflösung"
          hint="„Wie der Host“ streamt den Bildschirm des Hosts in seiner eigenen Größe."
        >
          <Segmented<SizeKey>
            className="flex-wrap"
            label="Auflösung"
            value={sizeKey(s)}
            onChange={(v) => {
              const [width, height] = SIZES.find((x) => x.value === v)!.size;
              update({ ...s, width, height });
            }}
            options={SIZES.map(({ value, label }) => ({ value, label }))}
          />
        </Setting>
        <Setting title="Bildrate" hint="Höher fühlt sich direkter an und braucht mehr Bitrate.">
          <Segmented<(typeof FPS)[number]>
            className="flex-wrap"
            label="Bildrate"
            value={(FPS.find((f) => Number(f) === s.fps) ?? "60") as (typeof FPS)[number]}
            onChange={(v) => update({ ...s, fps: Number(v) })}
            options={FPS.map((f) => ({ value: f, label: `${f} fps` }))}
          />
        </Setting>
        <Setting
          title="Bitrate"
          hint="Obergrenze. Der Host senkt sie selbst, wenn das Netz nicht mitkommt (WLAN), und hebt sie danach wieder an."
        >
          <Segmented<(typeof BITRATES)[number]["value"]>
            className="flex-wrap"
            label="Bitrate"
            value={
              (BITRATES.find((b) => Number(b.value) === s.bitrateMbit)?.value ??
                "0") as (typeof BITRATES)[number]["value"]
            }
            onChange={(v) => update({ ...s, bitrateMbit: Number(v) })}
            options={BITRATES.map(({ value, label }) => ({ value, label }))}
          />
        </Setting>
        <div>
          <button
            type="button"
            className="cursor-pointer border-0 bg-transparent p-0 text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
            onClick={() => update(DEFAULT_SETTINGS)}
          >
            Zurücksetzen
          </button>
        </div>
      </section>
      {inApp() && <HostSettings />}
    </>
  );
}

/** Settings of the host on this computer, when one runs. */
function HostSettings() {
  const queryClient = useQueryClient();
  const { data } = useQuery(thisMachineQuery);
  const boost = useMutation({
    mutationFn: actions.setGpuBoost,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["this-machine"] }),
  });
  const host = data?.host;
  if (!host) return null;
  return (
    <section className="flex max-w-[640px] flex-col gap-6 border-t border-border pt-6">
      <h2 className="m-0 text-lg font-semibold tracking-tight">Dieser Rechner als Host</h2>
      <Setting
        title="Grafikkarte während Sitzungen hochtakten"
        hint="Zwischen zwei Bildern taktet die Grafikkarte herunter, und das nächste Bild braucht zum Kodieren länger. Während eine Sitzung läuft, hält der Host sie deshalb auf vollem Takt und stellt danach alles zurück. Kostet nur in Sitzungen mehr Strom. Gilt für AMD und Intel."
      >
        <label className="flex cursor-pointer items-center gap-2">
          <input
            type="checkbox"
            checked={host.gpu_boost ?? true}
            disabled={boost.isPending}
            onChange={(e) => boost.mutate(e.target.checked)}
            className="m-0 size-4 accent-foreground"
          />
          Hochtakten
        </label>
      </Setting>
      {boost.isError && (
        <p role="alert" className="m-0 text-xs text-muted-foreground">
          {errorText(boost.error)}
        </p>
      )}
    </section>
  );
}

function Setting({
  title,
  hint,
  children,
}: {
  title: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-2">
      <h2 className="m-0 text-sm font-semibold">{title}</h2>
      <span className="text-xs leading-normal text-muted-foreground">{hint}</span>
      <div className="flex flex-wrap">{children}</div>
    </div>
  );
}
