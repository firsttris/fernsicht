// Takes the screenshots in docs/ from the demo data (scripts/screenshots.sh
// serves the builds). By hand: node screenshots.mjs <docs dir>
// The UI is German; the pictures show it as it is.
import { chromium, devices } from "@playwright/test";

const OUT = process.argv[2] ?? "../../docs";
const CLIENT = "http://localhost:4311";
const VIEWER = "http://localhost:4312";
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || undefined });

async function page(options) {
  const ctx = await browser.newContext({ locale: "de-DE", colorScheme: "dark", ...options });
  return ctx.newPage();
}

const desktop = await page({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1.5 });
const shot = async (p, name, opts = {}) => {
  await p.waitForTimeout(opts.wait ?? 1200);
  await p.screenshot({ path: `${OUT}/screenshot-${name}.png`, fullPage: !!opts.full });
  console.log("shot", name);
};

// The demo session has no stream; for the pictures, the remote screen shows
// a desktop (this app's device list, taken first).
const fillVideo = (p, png) =>
  p.evaluate(
    (src) => {
      const box = [...document.querySelectorAll("div")].find(
        (d) => d.childElementCount === 2 && d.textContent?.startsWith("Videostream von"),
      );
      if (!box) return;
      box.replaceChildren();
      Object.assign(box.style, {
        padding: "0",
        border: "0",
        overflow: "hidden",
        borderRadius: "6px",
        aspectRatio: "16 / 10",
      });
      const img = document.createElement("img");
      Object.assign(img, { src, alt: "" });
      Object.assign(img.style, {
        width: "100%",
        height: "100%",
        objectFit: "cover",
        display: "block",
      });
      box.append(img);
    },
    `data:image/png;base64,${png.toString("base64")}`,
  );

// The app: devices in the LAN, a session with the latency overlay, settings.
await desktop.goto(`${CLIENT}/devices`);
await desktop.getByRole("article").first().waitFor();
await shot(desktop, "devices");
const remoteDesktop = await desktop.screenshot();
await desktop.getByRole("button", { name: "Verbinden" }).click();
await desktop.keyboard.type("214776390");
await shot(desktop, "connect", { wait: 500 });
await desktop.keyboard.press("Escape");
await desktop.goto(`${CLIENT}/session/214776390`);
await desktop.getByRole("region", { name: "Latenz" }).waitFor();
await fillVideo(desktop, remoteDesktop);
await shot(desktop, "session", { wait: 2500 });
await desktop.goto(`${CLIENT}/settings`);
await shot(desktop, "settings");

// The web viewer in a browser, and on a phone.
await desktop.goto(VIEWER);
await shot(desktop, "viewer-connect");
const phone = await page({ ...devices["Pixel 7"], colorScheme: "dark" });
await phone.goto(`${VIEWER}/session/214776390`);
await phone.getByRole("toolbar").waitFor();
await fillVideo(phone, remoteDesktop);
await shot(phone, "viewer-phone", { wait: 2000 });
const landscape = await page({ ...devices["Pixel 7 landscape"], colorScheme: "dark" });
await landscape.goto(`${VIEWER}/session/214776390`);
await landscape.getByRole("toolbar").waitFor();
await fillVideo(landscape, remoteDesktop);
await shot(landscape, "viewer-phone-landscape", { wait: 2000 });

// Banner and social preview: the session picture under the name.
const art = await page({ viewport: { width: 1280, height: 640 }, deviceScaleFactor: 1 });
const session = (await import("node:fs"))
  .readFileSync(`${OUT}/screenshot-session.png`)
  .toString("base64");
const card = (
  width,
  height,
  title,
) => `<!doctype html><html><body style="margin:0;width:${width}px;height:${height}px;
  background:radial-gradient(circle at 20% 10%,#27272a,#09090b 60%);font-family:Geist,Inter,system-ui,sans-serif;color:#fafafa;
  display:flex;align-items:center;gap:48px;padding:0 64px;box-sizing:border-box;overflow:hidden">
  <div style="flex:0 0 auto;max-width:${title ? 520 : 600}px">
    <div style="display:flex;align-items:center;gap:16px">
      <svg width="56" height="56" viewBox="0 0 24 24" fill="none" stroke="#09090b" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"
        style="background:#fafafa;border-radius:12px;padding:10px;box-sizing:border-box">
        <path d="M2.06 12.35a1 1 0 0 1 0-.7 10.75 10.75 0 0 1 19.88 0 1 1 0 0 1 0 .7 10.75 10.75 0 0 1-19.88 0"/><circle cx="12" cy="12" r="3"/></svg>
      <span style="font-size:52px;font-weight:700;letter-spacing:-1px">Fernsicht</span>
    </div>
    <p style="font-size:26px;line-height:1.35;color:#d4d4d8;margin:24px 0 0">Low-latency remote desktop and game streaming for Linux.</p>
    <p style="font-size:18px;line-height:1.5;color:#a1a1aa;margin:16px 0 0">Self-hosted · Rust · Vulkan · VAAPI &amp; NVENC · AV1 / HEVC / H.264 · ~10&nbsp;ms in the LAN</p>
  </div>
  <img src="data:image/png;base64,${session}" style="height:${Math.round(height * 0.78)}px;border-radius:14px;border:1px solid #3f3f46;box-shadow:0 20px 60px rgba(0,0,0,.6)">
</body></html>`;
await art.setContent(card(1280, 640, true));
await art.waitForTimeout(300);
await art.screenshot({ path: `${OUT}/social-preview.png` });
console.log("shot social-preview");
await art.setViewportSize({ width: 1800, height: 560 });
await art.setContent(card(1800, 560, false));
await art.waitForTimeout(300);
await art.screenshot({ path: `${OUT}/banner.png` });
console.log("shot banner");

await browser.close();
