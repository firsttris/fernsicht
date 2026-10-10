/**
 * The UI inside the desktop app: data and actions come from the app's
 * commands (mocked here like the Rust backend answers).
 */
import type { Device } from "@fernsicht/ui";
import { act, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { HostService, HostStatus, SessionState, ThisMachine } from "./lib/api";
import { renderApp } from "./test-utils";

const backend = vi.hoisted(() => ({
  devices: [] as Device[],
  thisMachine: { name: "bazzite", host: null } as ThisMachine,
  session: { active: false } as SessionState,
  fail: {} as Record<string, string>,
  calls: [] as [string, unknown][],
}));

vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: async (cmd: string, args?: unknown) => {
    backend.calls.push([cmd, args]);
    if (backend.fail[cmd]) throw backend.fail[cmd];
    switch (cmd) {
      case "devices":
        // Fresh objects each time, as from the real backend.
        return structuredClone(backend.devices);
      case "this_machine":
        return backend.thisMachine;
      case "session":
        return backend.session;
      case "pair": {
        const { address } = args as { address: string };
        const d = backend.devices.find((d) => d.address?.startsWith(address));
        if (d) Object.assign(d, { paired: true, pairing: false });
        return d;
      }
      case "connect":
        backend.session = { active: true, deviceId: "k1", deviceName: "zentrale", stats: null };
        return backend.session;
      case "forget":
        backend.devices = backend.devices.filter((d) => d.id !== (args as { id: string }).id);
        return null;
      case "set_muted":
      case "set_mode":
      case "send_keys":
        return null;
      case "set_gpu_boost":
        backend.thisMachine = {
          ...backend.thisMachine,
          host: host({ gpu_boost: (args as { on: boolean }).on }),
        };
        return null;
      case "share_this_machine":
        backend.thisMachine = {
          ...backend.thisMachine,
          host: host(),
          service: { ...service(), installed: true, active: true, version: "0.2.0" },
        };
        return null;
      case "stop_sharing":
        backend.thisMachine = { ...backend.thisMachine, host: null, service: service() };
        return null;
      case "disconnect":
        backend.session = { ...backend.session, active: false };
        return null;
      case "open_pairing":
        return { pin: "482913", expires_in_s: 300 };
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  },
}));

const zentrale = (over: Partial<Device> = {}): Device => ({
  id: "k1",
  name: "zentrale",
  online: true,
  favorite: false,
  os: "Bazzite",
  gpu: "Radeon RX 7700 XT / 7800 XT · H.264",
  paired: true,
  pairing: false,
  busy: false,
  address: "192.168.178.87:47800",
  ...over,
});

const service = (over: Partial<HostService> = {}): HostService => ({
  installed: false,
  active: false,
  version: null,
  bundled: "0.2.0",
  canInstall: true,
  ...over,
});

const host = (over: Partial<HostStatus> = {}): HostStatus => ({
  name: "bazzite",
  key: "aaaa",
  paired: [{ name: "zentrale", key: "bbbb" }],
  session: null,
  pairing: null,
  ...over,
});

beforeEach(() => {
  backend.devices = [];
  backend.thisMachine = { name: "bazzite", host: null };
  backend.session = { active: false };
  backend.fail = {};
  backend.calls = [];
});
afterEach(() => vi.useRealTimers());

const called = (cmd: string) => backend.calls.filter(([c]) => c === cmd);

describe("Desktop-App", () => {
  it("lists hosts in the LAN with address, OS and GPU", async () => {
    backend.devices = [
      zentrale(),
      zentrale({ id: "k2", name: "buero", online: false, address: "192.168.178.50:47800" }),
    ];
    await renderApp("/devices");
    const card = (await screen.findByText("zentrale")).closest("article")!;
    expect(within(card).getByText("192.168.178.87")).toBeInTheDocument();
    expect(within(card).getByText("Radeon RX 7700 XT / 7800 XT · H.264")).toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "Desktop" })).toBeEnabled();
    const buero = screen.getByText("buero").closest("article")!;
    expect(within(buero).getByRole("button", { name: "Desktop" })).toBeDisabled();
    // No account-id connect without the rendezvous server.
    expect(screen.queryByRole("button", { name: "Verbinden" })).not.toBeInTheDocument();
  });

  it("finds devices by address", async () => {
    backend.devices = [
      zentrale(),
      zentrale({ id: "k2", name: "buero", address: "192.168.178.50:47800" }),
    ];
    const { user } = await renderApp("/devices");
    await screen.findByText("buero");
    await user.type(screen.getByPlaceholderText("Name oder Adresse suchen …"), "178.50");
    expect(
      screen.getAllByRole("article").map((a) => within(a).getByRole("heading").textContent),
    ).toEqual(["buero"]);
  });

  it("says when no host answers", async () => {
    await renderApp("/devices");
    expect(await screen.findByText(/Kein Fernsicht-Host im Netzwerk gefunden/)).toBeInTheDocument();
  });

  it("pairs with a host using its PIN", async () => {
    backend.devices = [zentrale({ paired: false, pairing: true })];
    const { user } = await renderApp("/devices");
    const card = (await screen.findByText("zentrale")).closest("article")!;
    expect(within(card).getByText("Nicht gekoppelt")).toBeInTheDocument();
    expect(within(card).getByText("Kopplung offen")).toBeInTheDocument();
    await user.click(within(card).getByRole("button", { name: "Koppeln" }));
    const dialog = screen.getByRole("dialog", { name: "Gerät koppeln" });
    expect(within(dialog).getByLabelText("Host")).toHaveValue("192.168.178.87:47800");
    const submit = within(dialog).getByRole("button", { name: "Koppeln" });
    expect(submit).toBeDisabled();
    await user.type(within(dialog).getByLabelText("PIN"), "48a29 13");
    expect(within(dialog).getByLabelText("PIN")).toHaveValue("482913");
    await user.click(submit);
    await waitFor(() =>
      expect(called("pair")).toEqual([
        ["pair", { address: "192.168.178.87:47800", pin: "482913" }],
      ]),
    );
    // Paired now: the card offers a session.
    expect(await within(card).findByRole("button", { name: "Desktop" })).toBeInTheDocument();
  });

  it("explains a wrong PIN", async () => {
    backend.fail.pair = "wrong PIN";
    const { user } = await renderApp("/devices");
    await screen.findByText(/Kein Fernsicht-Host/);
    await user.click(screen.getByRole("button", { name: "Gerät hinzufügen" }));
    const dialog = screen.getByRole("dialog", { name: "Gerät koppeln" });
    await user.type(within(dialog).getByLabelText("Host"), "192.168.178.87");
    await user.type(within(dialog).getByLabelText("PIN"), "000000");
    await user.click(within(dialog).getByRole("button", { name: "Koppeln" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("Falsche PIN.");
    await user.click(within(dialog).getByRole("button", { name: "Abbrechen" }));
  });

  it("starts a session in the native window and ends it", async () => {
    backend.devices = [zentrale()];
    const { router, user } = await renderApp("/devices");
    const card = (await screen.findByText("zentrale")).closest("article")!;
    await user.click(within(card).getByRole("button", { name: "Gaming" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/session/k1"));
    expect(router.state.location.search).toEqual({ mode: "gaming" });
    expect(called("connect")).toEqual([
      [
        "connect",
        {
          id: "k1",
          settings: { width: 0, height: 0, fps: 60, bitrateMbit: 0, codec: "auto", gaming: true },
        },
      ],
    ]);
    expect(await screen.findByText(/läuft in einem eigenen Fenster/)).toBeInTheDocument();
    // The session's controls reach the client.
    await user.click(screen.getByRole("button", { name: "Ton ausschalten" }));
    expect(called("set_muted")).toEqual([["set_muted", { muted: true }]]);
    await user.click(screen.getByRole("button", { name: "Desktop" }));
    expect(called("set_mode")).toEqual([["set_mode", { gaming: false }]]);
    await user.click(screen.getByRole("button", { name: "Tasten senden" }));
    await user.click(screen.getByRole("menuitem", { name: "Windows+W" }));
    expect(called("send_keys")).toEqual([["send_keys", { codes: [125, 17] }]]);
    await user.click(screen.getByRole("button", { name: "Trennen" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/devices"));
    expect(called("disconnect")).toHaveLength(1);
  });

  it("forgets a paired device after asking", async () => {
    backend.devices = [zentrale()];
    const { user } = await renderApp("/devices");
    const card = (await screen.findByText("zentrale")).closest("article")!;
    await user.click(within(card).getByRole("button", { name: "Gerät vergessen" }));
    expect(within(card).getByText("zentrale vergessen?")).toBeInTheDocument();
    await user.click(within(card).getByRole("button", { name: "Nein" }));
    expect(called("forget")).toEqual([]);
    await user.click(within(card).getByRole("button", { name: "Gerät vergessen" }));
    await user.click(within(card).getByRole("button", { name: "Vergessen" }));
    expect(called("forget")).toEqual([["forget", { id: "k1" }]]);
    expect(await screen.findByText(/Kein Fernsicht-Host im Netzwerk gefunden/)).toBeInTheDocument();
  });

  it("uses the settings for new sessions", async () => {
    backend.devices = [zentrale()];
    const { user } = await renderApp("/settings");
    await user.click(await screen.findByRole("radio", { name: "1080p" }));
    await user.click(screen.getByRole("radio", { name: "120 fps" }));
    await user.click(screen.getByRole("radio", { name: "20 Mbit/s" }));
    await user.click(screen.getByRole("radio", { name: "HEVC" }));
    await user.click(screen.getByRole("link", { name: "Geräte" }));
    const card = (await screen.findByText("zentrale")).closest("article")!;
    await user.click(within(card).getByRole("button", { name: "Desktop" }));
    await waitFor(() =>
      expect(called("connect")).toEqual([
        [
          "connect",
          {
            id: "k1",
            settings: {
              width: 1920,
              height: 1080,
              fps: 120,
              bitrateMbit: 20,
              codec: "hevc",
              gaming: false,
            },
          },
        ],
      ]),
    );
    localStorage.clear();
  });

  it("shows why a connection failed", async () => {
    backend.devices = [zentrale()];
    backend.fail.connect = "starting fernsicht-client: No such file or directory";
    const { user } = await renderApp("/devices");
    const card = (await screen.findByText("zentrale")).closest("article")!;
    await user.click(within(card).getByRole("button", { name: "Desktop" }));
    expect(await within(card).findByRole("alert")).toHaveTextContent("No such file");
  });

  it("shows the overlay and an ended session with its reason", async () => {
    backend.session = {
      active: true,
      deviceId: "k1",
      deviceName: "zentrale",
      stats: {
        stagesUs: { capture: 400, encode: 2_000, network: 1_500, decode: 900, present: 3_000 },
        codec: "H.264",
        fps: 60,
        bitrateBps: 35_000_000,
        lossBeforeFec: 0,
        lossAfterFec: 0,
      },
    };
    const { router, user } = await renderApp("/session/k1");
    expect(await screen.findByRole("region", { name: "Latenz" })).toBeInTheDocument();
    expect(screen.getByText("H.264")).toBeInTheDocument();
    backend.session = {
      active: false,
      deviceId: "k1",
      deviceName: "zentrale",
      error: "192.168.178.87:47800: this device is not paired with the host (…)",
    };
    expect(
      await screen.findByRole(
        "heading",
        { name: "Sitzung mit zentrale beendet" },
        { timeout: 3000 },
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("Bitte neu koppeln");
    await user.click(screen.getByRole("button", { name: "Zurück zu den Geräten" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/devices"));
  });

  it("shows this computer without a host", async () => {
    await renderApp("/devices");
    expect(await screen.findByText("bazzite")).toBeInTheDocument();
    expect(screen.getByText(/Kein Host aktiv/)).toBeInTheDocument();
  });

  it("opens pairing on this computer's host and shows the PIN", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    backend.thisMachine = { name: "zentrale", host: host() };
    const { user } = await renderApp("/devices");
    expect(await screen.findByText(/Host aktiv · 1 Gerät gekoppelt/)).toBeInTheDocument();
    backend.thisMachine = { name: "zentrale", host: host({ pairing: 300 }) };
    await user.click(screen.getByRole("button", { name: "Gerät koppeln" }));
    expect(await screen.findByText("482 913")).toBeInTheDocument();
    expect(screen.getByText("gilt noch 5:00")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(61_000));
    expect(screen.getByText("gilt noch 3:59")).toBeInTheDocument();
    // Paired: the host closed pairing, the PIN goes away.
    backend.thisMachine = {
      name: "zentrale",
      host: host({
        paired: [...host().paired, { name: "sofa", key: "cccc" }],
        session: { client: "sofa", address: "192.168.178.20:5000" },
      }),
    };
    expect(
      await screen.findByText(/2 Geräte gekoppelt · verbunden mit sofa/, undefined, {
        timeout: 5000,
      }),
    ).toBeInTheDocument();
    expect(screen.queryByText("482 913")).not.toBeInTheDocument();
  });

  it("shares this computer from the AppImage and stops sharing", async () => {
    backend.thisMachine = { name: "zentrale", host: null, service: service() };
    const { user } = await renderApp("/devices");
    expect(
      await screen.findByText(/fragt einmal nach dem Administrator-Passwort/),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Diesen Rechner freigeben" }));
    expect(called("share_this_machine")).toHaveLength(1);
    expect(await screen.findByText(/Host aktiv/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Host aktualisieren/ })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Freigabe beenden" }));
    await user.click(screen.getByRole("button", { name: "Nein" }));
    expect(called("stop_sharing")).toHaveLength(0);
    await user.click(screen.getByRole("button", { name: "Freigabe beenden" }));
    await user.click(screen.getByRole("button", { name: "Beenden" }));
    expect(called("stop_sharing")).toHaveLength(1);
    expect(
      await screen.findByRole("button", { name: "Diesen Rechner freigeben" }),
    ).toBeInTheDocument();
  });

  it("offers an update when the AppImage carries a newer host", async () => {
    backend.thisMachine = {
      name: "zentrale",
      host: host(),
      service: service({ installed: true, active: true, version: "0.1.0" }),
    };
    const { user } = await renderApp("/devices");
    await user.click(await screen.findByRole("button", { name: "Host aktualisieren auf 0.2.0" }));
    expect(called("share_this_machine")).toHaveLength(1);
  });

  it("says when sharing is set up but the host does not run", async () => {
    backend.thisMachine = {
      name: "zentrale",
      host: null,
      service: service({ installed: true, version: "0.2.0" }),
    };
    await renderApp("/devices");
    expect(await screen.findByText(/der Host läuft aber nicht/)).toBeInTheDocument();
  });

  it("says the password prompt was cancelled", async () => {
    backend.thisMachine = { name: "zentrale", host: null, service: service() };
    backend.fail.share_this_machine = "cancelled";
    const { user } = await renderApp("/devices");
    await user.click(await screen.findByRole("button", { name: "Diesen Rechner freigeben" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Abgebrochen.");
  });

  it("switches the host's GPU boost in the settings", async () => {
    backend.thisMachine = { name: "zentrale", host: host({ gpu_boost: true }) };
    const { user } = await renderApp("/settings");
    const box = await screen.findByRole("checkbox", { name: "Hochtakten" });
    expect(box).toBeChecked();
    await user.click(box);
    expect(called("set_gpu_boost")).toEqual([["set_gpu_boost", { on: false }]]);
    await waitFor(() =>
      expect(screen.getByRole("checkbox", { name: "Hochtakten" })).not.toBeChecked(),
    );
  });

  it("shows no host settings without a host", async () => {
    await renderApp("/settings");
    await screen.findByRole("radio", { name: "Wie der Host" });
    expect(screen.queryByText("Dieser Rechner als Host")).not.toBeInTheDocument();
  });

  it("explains a failed pairing request on this computer", async () => {
    backend.thisMachine = { name: "zentrale", host: host() };
    backend.fail.open_pairing = "not allowed: only root and the desktop's user";
    const { user } = await renderApp("/devices");
    await user.click(await screen.findByRole("button", { name: "Gerät koppeln" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("not allowed");
  });
});
