import { screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { renderApp } from "../test-utils";

afterEach(() => vi.restoreAllMocks());

describe("App-Layout", () => {
  it("highlights the active section", async () => {
    await renderApp("/devices");
    const nav = screen.getByRole("navigation", { name: "Hauptnavigation" });
    expect(nav.querySelector('a[href="/devices"]')).toHaveClass("bg-secondary");
    expect(nav.querySelector('a[href="/history"]')).not.toHaveClass("bg-secondary");
  });

  it.each([
    ["Verlauf", "/history", "Kommt mit dem Audit-Log in Phase 4."],
    ["Zugriffe & Rechte", "/access", "Kommt mit den Session-Typen in Phase 3."],
  ])("navigates to %s", async (label, path, note) => {
    const { router, user } = await renderApp("/devices");
    await user.click(screen.getByRole("link", { name: label }));
    await waitFor(() => expect(router.state.location.pathname).toBe(path));
    expect(screen.getByRole("heading", { level: 1, name: label })).toBeInTheDocument();
    expect(screen.getByText(note)).toBeInTheDocument();
  });

  it("shows this machine's id and code", async () => {
    await renderApp("/devices");
    expect(await screen.findByText("482 913 057")).toBeInTheDocument();
    expect(screen.getByText("Code: k7f-2qx")).toBeInTheDocument();
  });

  it("copies id and code to the clipboard", async () => {
    const { user } = await renderApp("/devices");
    const writeText = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    await user.click(await screen.findByRole("button", { name: "ID und Code kopieren" }));
    expect(writeText).toHaveBeenCalledWith("482 913 057 · k7f-2qx");
    expect(await screen.findByRole("button", { name: "Kopiert" })).toBeInTheDocument();
    await waitFor(
      () => expect(screen.getByRole("button", { name: "ID und Code kopieren" })).toBeVisible(),
      { timeout: 3000 },
    );
  });

  it("survives a clipboard that refuses", async () => {
    const { user } = await renderApp("/devices");
    vi.spyOn(navigator.clipboard, "writeText").mockRejectedValue(new Error("denied"));
    await user.click(await screen.findByRole("button", { name: "ID und Code kopieren" }));
    expect(screen.getByRole("button", { name: "ID und Code kopieren" })).toBeInTheDocument();
  });
});
