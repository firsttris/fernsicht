import { Button, Input, Label } from "@fernsicht/ui";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { actions, errorText, type InternetStatus } from "../lib/api";

/** How the host stands with its rendezvous service, in words. */
const STATE_TEXT: Record<InternetStatus["state"], (s: InternetStatus) => string> = {
  off: () => "Aus: nur im LAN und über ein VPN erreichbar.",
  connecting: () => "Verbinde mit dem Rendezvous-Dienst …",
  registered: (s) =>
    s.relay
      ? "Erreichbar über das Internet, mit Relay für Netze ohne direkten Weg."
      : "Erreichbar über das Internet (ohne Relay: nur wo ein direkter Weg geht).",
  failed: (s) =>
    `Der Rendezvous-Dienst ist nicht erreichbar${s.error ? ` (${s.error})` : ""}. Wird erneut versucht.`,
};

/**
 * The host's rendezvous service (docs/internet.md): with one, paired
 * devices reach this computer from anywhere. Each user runs their own (a
 * Cloudflare Worker or the self-hosted service); devices learn the
 * address from the host.
 */
export function InternetSetting({ internet }: { internet: InternetStatus }) {
  const queryClient = useQueryClient();
  const [url, setUrl] = useState(internet.url ?? "");
  const save = useMutation({
    mutationFn: actions.setRendezvous,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["this-machine"] }),
  });
  const changed = url.trim() !== (internet.url ?? "");

  return (
    <div className="flex flex-col gap-2">
      <h2 className="m-0 text-sm font-semibold">Verbindung über das Internet</h2>
      <span className="text-xs leading-normal text-muted-foreground">
        Mit einem eigenen Rendezvous-Dienst erreichen gekoppelte Geräte diesen Rechner von überall,
        ohne VPN und ohne Portfreigabe. Das Bild läuft direkt zwischen den Geräten, nur wo das nicht
        geht über ein Relay. Einrichten in wenigen Minuten mit einem kostenlosen Cloudflare-Konto:
        siehe die Anleitung „Over the internet“ in der Dokumentation.
      </span>
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate(url.trim());
        }}
      >
        <div className="flex min-w-[260px] flex-1 flex-col gap-1.5">
          <Label htmlFor="rendezvous-url">Adresse des Rendezvous-Dienstes</Label>
          <Input
            id="rendezvous-url"
            type="url"
            autoComplete="off"
            spellCheck={false}
            placeholder="https://fernsicht-rendezvous.name.workers.dev"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
        </div>
        <Button type="submit" size="sm" disabled={!changed || save.isPending}>
          Speichern
        </Button>
        {internet.url && (
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={save.isPending}
            onClick={() => {
              setUrl("");
              save.mutate("");
            }}
          >
            Entfernen
          </Button>
        )}
      </form>
      <span className="text-xs text-muted-foreground" aria-live="polite">
        {STATE_TEXT[internet.state](internet)}
      </span>
      {save.isError && (
        <span role="alert" className="text-xs text-muted-foreground">
          {errorText(save.error)}
        </span>
      )}
    </div>
  );
}
