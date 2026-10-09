import {
  Outlet,
  type RouterHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";

import { ConnectPage } from "./routes/connect";
import { ViewerSessionPage } from "./routes/session";

const rootRoute = createRootRoute({ component: Outlet });

const connectRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  // Shared links carry the device ID: /?id=214776390
  validateSearch: (search: Record<string, unknown>): { id?: string } =>
    typeof search.id === "string" || typeof search.id === "number" ? { id: String(search.id) } : {},
  component: ConnectPage,
});

const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/session/$deviceId",
  validateSearch: (search: Record<string, unknown>): { mode: "desktop" | "gaming" } => ({
    mode: search.mode === "gaming" ? "gaming" : "desktop",
  }),
  component: ViewerSessionPage,
});

const routeTree = rootRoute.addChildren([connectRoute, sessionRoute]);

/** Browser history by default; tests pass a memory history. */
export function createViewerRouter(history?: RouterHistory) {
  return createRouter({ routeTree, history });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createViewerRouter>;
  }
}
