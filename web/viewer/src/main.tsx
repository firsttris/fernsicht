import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  Outlet,
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import "./index.css";
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

const router = createRouter({
  routeTree: rootRoute.addChildren([connectRoute, sessionRoute]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={new QueryClient()}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
