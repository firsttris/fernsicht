import {
  Outlet,
  type RouterHistory,
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
} from "@tanstack/react-router";

import { AppLayout } from "./routes/app-layout";
import { DevicesPage } from "./routes/devices";
import { PlaceholderPage } from "./routes/placeholder";
import { SessionPage } from "./routes/session";

const rootRoute = createRootRoute({ component: Outlet });

const appRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "app",
  component: AppLayout,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/devices" });
  },
});

const devicesRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/devices",
  component: DevicesPage,
});

const historyRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/history",
  component: () => (
    <PlaceholderPage
      title="Verlauf"
      description="Vergangene Sitzungen mit Dauer, Modus und Latenz."
      phase="Kommt mit dem Audit-Log in Phase 4."
    />
  ),
});

const accessRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/access",
  component: () => (
    <PlaceholderPage
      title="Zugriffe & Rechte"
      description="Wer darf auf welche Geräte zugreifen, mit welchem Session-Typ (View, Control, Files, Gaming)."
      phase="Kommt mit den Session-Typen in Phase 3."
    />
  ),
});

const settingsRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/settings",
  component: () => (
    <PlaceholderPage
      title="Einstellungen"
      description="Auflösung, Bildrate, Bitrate, Codec und Tastenkürzel."
      phase="Kommt mit der Produkt-Hülle in Phase 4."
    />
  ),
});

export type SessionSearch = { mode: "desktop" | "gaming" };

const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/session/$deviceId",
  validateSearch: (search: Record<string, unknown>): SessionSearch => ({
    mode: search.mode === "gaming" ? "gaming" : "desktop",
  }),
  component: SessionPage,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  appRoute.addChildren([devicesRoute, historyRoute, accessRoute, settingsRoute]),
  sessionRoute,
]);

/** Browser history by default; tests pass a memory history. */
export function createAppRouter(history?: RouterHistory) {
  return createRouter({ routeTree, history, defaultPreload: "intent" });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
