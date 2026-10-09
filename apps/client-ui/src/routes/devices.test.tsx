import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderApp } from "../test-utils";

async function cards() {
  await screen.findByText("zentrale");
  return screen.getAllByRole("article");
}

const names = (articles: HTMLElement[]) =>
  articles.map((a) => within(a).getByRole("heading").textContent);

describe("Geräte", () => {
  it("redirects / to the device list", async () => {
    const { router } = await renderApp("/");
    expect(router.state.location.pathname).toBe("/devices");
  });

  it("lists all devices with status, OS and GPU", async () => {
    await renderApp("/devices");
    const list = await cards();
    expect(names(list)).toEqual(["zentrale", "heimserver", "werkstatt-pc", "testrechner"]);
    const zentrale = list[0]!;
    expect(within(zentrale).getByText("214 776 390")).toBeInTheDocument();
    expect(within(zentrale).getByText("Online")).toBeInTheDocument();
    expect(within(zentrale).getByText("RX 7800 XT · AV1")).toBeInTheDocument();
  });

  it("filters by name and by id (spaces ignored)", async () => {
    const { user } = await renderApp("/devices");
    await cards();
    const search = screen.getByRole("searchbox", { name: "Geräte durchsuchen" });
    await user.type(search, "HEIM");
    expect(names(screen.getAllByRole("article"))).toEqual(["heimserver"]);
    await user.clear(search);
    await user.type(search, "557 204");
    expect(names(screen.getAllByRole("article"))).toEqual(["werkstatt-pc"]);
    await user.clear(search);
    await user.type(search, "gibtsnicht");
    expect(screen.queryAllByRole("article")).toHaveLength(0);
    expect(screen.getByText("Keine Geräte gefunden.")).toBeInTheDocument();
  });

  it("filters online devices and favourites", async () => {
    const { user } = await renderApp("/devices");
    await cards();
    await user.click(screen.getByRole("radio", { name: "Online" }));
    expect(names(screen.getAllByRole("article"))).toEqual(["zentrale", "heimserver"]);
    await user.click(screen.getByRole("radio", { name: "Favoriten" }));
    expect(names(screen.getAllByRole("article"))).toEqual(["zentrale", "werkstatt-pc"]);
    await user.click(screen.getByRole("radio", { name: "Alle" }));
    expect(screen.getAllByRole("article")).toHaveLength(4);
  });

  it("disables connecting to offline devices", async () => {
    await renderApp("/devices");
    const offline = (await cards())[2]!;
    for (const name of ["Desktop", "Gaming"]) {
      expect(within(offline).getByRole("button", { name })).toBeDisabled();
    }
  });

  it.each([
    ["Desktop", "desktop"],
    ["Gaming", "gaming"],
  ])("starts a %s session from a card", async (label, mode) => {
    const { router, user } = await renderApp("/devices");
    const zentrale = (await cards())[0]!;
    await user.click(within(zentrale).getByRole("link", { name: label }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/session/214776390"));
    expect(router.state.location.search).toEqual({ mode });
  });

  it("keeps pairing disabled until the rendezvous server exists", async () => {
    await renderApp("/devices");
    expect(screen.getByRole("button", { name: "Gerät hinzufügen" })).toBeDisabled();
  });
});

describe("Verbinden-Dialog", () => {
  it("connects to a typed device id", async () => {
    const { router, user } = await renderApp("/devices");
    await user.click(screen.getByRole("button", { name: "Verbinden" }));
    const dialog = screen.getByRole("dialog", { name: "Mit einem Rechner verbinden" });
    const input = within(dialog).getByLabelText("Geräte-ID");
    const submit = within(dialog).getByRole("button", { name: "Verbinden" });

    expect(submit).toBeDisabled();
    await user.type(input, "9031184");
    expect(input).toHaveValue("903 118 4");
    expect(submit).toBeDisabled();
    await user.type(input, "52999");
    expect(input).toHaveValue("903 118 452"); // capped at nine digits
    expect(submit).toBeEnabled();

    await user.click(submit);
    await waitFor(() => expect(router.state.location.pathname).toBe("/session/903118452"));
  });

  it("can be cancelled", async () => {
    const { user } = await renderApp("/devices");
    await user.click(screen.getByRole("button", { name: "Verbinden" }));
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("open");
    await user.click(within(dialog).getByRole("button", { name: "Abbrechen" }));
    expect(dialog).not.toHaveAttribute("open");
  });
});
