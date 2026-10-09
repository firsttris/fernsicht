import { expect, test } from "@playwright/test";

import { CLIENT, expectAccessible, expectNoHorizontalScroll } from "./helpers";

test.describe("Client-UI", () => {
  test("device list: search, filter, connect", async ({ page }) => {
    await page.goto(CLIENT);
    await expect(page).toHaveURL(/\/devices$/);
    const cards = page.getByRole("article");
    await expect(cards).toHaveCount(4);

    await page.getByRole("searchbox", { name: "Geräte durchsuchen" }).fill("214 776");
    await expect(cards).toHaveCount(1);
    await expect(cards.first()).toContainText("zentrale");
    await page.getByRole("searchbox").clear();

    await page.getByRole("radio", { name: "Online" }).click();
    await expect(cards).toHaveCount(2);

    await cards.first().getByRole("link", { name: "Gaming" }).click();
    await expect(page).toHaveURL(/\/session\/214776390\?mode=gaming$/);
    await expect(page.getByRole("button", { name: "Gaming" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
  });

  test("connect dialog works with the keyboard", async ({ page }) => {
    await page.goto(`${CLIENT}/devices`);
    await page.getByRole("button", { name: "Verbinden" }).click();
    const dialog = page.getByRole("dialog", { name: "Mit einem Rechner verbinden" });
    await expect(dialog).toBeVisible();
    await page.keyboard.type("903118452");
    await expect(dialog.getByLabel("Geräte-ID")).toHaveValue("903 118 452");
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/session\/903118452/);
    await expect(page.getByText("heimserver", { exact: true })).toBeVisible();
  });

  test("escape closes the connect dialog", async ({ page }) => {
    await page.goto(`${CLIENT}/devices`);
    await page.getByRole("button", { name: "Verbinden" }).click();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toBeHidden();
  });

  test("session: live overlay, keyboard toggle, disconnect", async ({ page }) => {
    await page.goto(`${CLIENT}/session/214776390`);
    const overlay = page.getByRole("region", { name: "Latenz" });
    await expect(overlay).toBeVisible();
    for (const label of ["Capture", "Encode", "Netz", "Decode", "Anzeige", "Verlust (FEC)"]) {
      await expect(overlay.getByText(label)).toBeVisible();
    }
    // Stats refresh every second.
    const first = await overlay.textContent();
    await expect.poll(() => overlay.textContent(), { timeout: 5_000 }).not.toBe(first);

    await page.keyboard.press("Control+Alt+f");
    await expect(page.getByRole("toolbar")).toBeHidden();
    await page.keyboard.press("Control+Alt+f");
    await expect(page.getByRole("toolbar")).toBeVisible();

    await page.getByRole("button", { name: "Trennen" }).click();
    await expect(page).toHaveURL(/\/devices$/);
  });

  test("navigation to the other sections", async ({ page }) => {
    await page.goto(`${CLIENT}/devices`);
    for (const [label, path] of [
      ["Verlauf", "/history"],
      ["Zugriffe & Rechte", "/access"],
      ["Einstellungen", "/settings"],
      ["Geräte", "/devices"],
    ] as const) {
      await page.getByRole("link", { name: label }).click();
      await expect(page).toHaveURL(new RegExp(`${path}$`));
      await expect(page.getByRole("heading", { level: 1, name: label })).toBeVisible();
    }
  });

  test("deep links survive a reload", async ({ page }) => {
    await page.goto(`${CLIENT}/session/557204819?mode=gaming`);
    await page.reload();
    await expect(page.getByText("werkstatt-pc", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Gaming" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
  });

  for (const path of ["/devices", "/session/214776390", "/settings"]) {
    test(`accessible and no sideways scroll: ${path}`, async ({ page }) => {
      await page.goto(`${CLIENT}${path}`);
      await expect(
        page.getByRole(path.startsWith("/session") ? "toolbar" : "navigation"),
      ).toBeVisible();
      await expectAccessible(page);
      await expectNoHorizontalScroll(page);
    });
  }

  test("connect dialog is accessible", async ({ page }) => {
    await page.goto(`${CLIENT}/devices`);
    await page.getByRole("button", { name: "Verbinden" }).click();
    await expect(page.getByRole("dialog")).toBeVisible();
    await expectAccessible(page);
  });
});
