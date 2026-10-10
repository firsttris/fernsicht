import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderViewer } from "../test-utils";

describe("Web-Viewer: Verbinden", () => {
  it("prefills the device id from a shared link", async () => {
    await renderViewer("/?id=214776390");
    expect(screen.getByLabelText("Geräte-ID")).toHaveValue("214 776 390");
  });

  it("accepts numeric ids in links", async () => {
    // TanStack parses ?id=214776390 as a number.
    await renderViewer("/?id=214776390");
    expect(screen.getByLabelText("Geräte-ID")).toHaveValue("214 776 390");
  });

  it("requires a full id and a one-time code", async () => {
    const { user } = await renderViewer("/");
    const submit = screen.getByRole("button", { name: "Verbinden" });
    expect(submit).toBeDisabled();
    await user.type(screen.getByLabelText("Geräte-ID"), "214776390");
    expect(submit).toBeDisabled();
    await user.type(screen.getByLabelText("Einmal-Code"), "K7F-2QX");
    expect(screen.getByLabelText("Einmal-Code")).toHaveValue("k7f-2qx");
    expect(submit).toBeEnabled();
    await user.clear(screen.getByLabelText("Einmal-Code"));
    await user.type(screen.getByLabelText("Einmal-Code"), "k7f2qx");
    expect(submit).toBeDisabled();
  });

  it("connects in the chosen mode", async () => {
    const { router, user } = await renderViewer("/?id=214776390");
    await user.type(screen.getByLabelText("Einmal-Code"), "abc-123");
    expect(screen.getByRole("radio", { name: /Desktop/ })).toBeChecked();
    await user.click(screen.getByRole("radio", { name: /Gaming/ }));
    await user.click(screen.getByRole("button", { name: "Verbinden" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/session/214776390"));
    expect(router.state.location.search).toEqual({ mode: "gaming" });
  });

  it("does not submit an incomplete form with Enter", async () => {
    const { router, user } = await renderViewer("/");
    await user.type(screen.getByLabelText("Geräte-ID"), "123{Enter}");
    expect(router.state.location.pathname).toBe("/");
  });
});

describe("Web-Viewer: Session", () => {
  it("names known devices and returns to the form with the id", async () => {
    const { router, user } = await renderViewer("/session/214776390?mode=gaming");
    expect(screen.getByText("zentrale")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Gaming" })).toHaveAttribute("aria-pressed", "true");
    await user.click(screen.getByRole("button", { name: "Desktop" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ mode: "desktop" }));
    await user.click(screen.getByRole("button", { name: "Trennen" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(router.state.location.search).toEqual({ id: "214776390" });
  });

  it("shows unknown devices by formatted id", async () => {
    await renderViewer("/session/111222333");
    expect(screen.getByText("111 222 333")).toBeInTheDocument();
    expect(await screen.findByRole("region", { name: "Latenz" })).toBeInTheDocument();
  });
});
