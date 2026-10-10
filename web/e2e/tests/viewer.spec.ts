import { expect, test } from "@playwright/test";

import { VIEWER, expectAccessible, expectNoHorizontalScroll } from "./helpers";

test.describe("Web-Viewer", () => {
  test("connect from a shared link in gaming mode", async ({ page }) => {
    await page.goto(`${VIEWER}/?id=214776390`);
    const form = page.getByRole("form", { name: "Mit einem Rechner verbinden" });
    await expect(form.getByLabel("Geräte-ID")).toHaveValue("214 776 390");
    const connect = form.getByRole("button", { name: "Verbinden" });
    await expect(connect).toBeDisabled();

    await form.getByLabel("Einmal-Code").fill("K7F-2QX");
    await expect(form.getByLabel("Einmal-Code")).toHaveValue("k7f-2qx");
    await form.getByText("Gaming", { exact: true }).click();
    await connect.click();

    await expect(page).toHaveURL(/\/session\/214776390\?mode=gaming$/);
    await expect(page.getByText("zentrale", { exact: true })).toBeVisible();
    await expect(page.getByRole("region", { name: "Latenz" })).toBeVisible();
  });

  test("disconnect returns to the form with the id kept", async ({ page }) => {
    await page.goto(`${VIEWER}/session/903118452`);
    await page.getByRole("button", { name: "Trennen" }).click();
    await expect(page.getByLabel("Geräte-ID")).toHaveValue("903 118 452");
  });

  test("the form is accessible and fits a phone", async ({ page }) => {
    await page.goto(VIEWER);
    await expect(page.getByRole("form")).toBeVisible();
    await expectAccessible(page);
    await expectNoHorizontalScroll(page);
  });

  test("the session view is accessible and fits the screen", async ({ page }) => {
    await page.goto(`${VIEWER}/session/214776390`);
    await expect(page.getByRole("toolbar")).toBeVisible();
    await expectAccessible(page);
    await expectNoHorizontalScroll(page);
    // Toolbar and its buttons stay on screen, also with little height.
    const bar = await page.getByRole("toolbar").boundingBox();
    const view = page.viewportSize()!;
    expect(bar!.y + bar!.height).toBeLessThanOrEqual(view.height);
    await expect(page.getByRole("button", { name: "Trennen" })).toBeInViewport();
  });
});
