import { QueryClient } from "@tanstack/react-query";
import { createMemoryHistory } from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { App } from "./app";
import { createAppRouter } from "./router";

/** Renders the whole client UI at `path` with in-memory history. */
export async function renderApp(path: string) {
  const router = createAppRouter(createMemoryHistory({ initialEntries: [path] }));
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const user = userEvent.setup();
  render(<App router={router} queryClient={queryClient} />);
  // Wait for the first route to render.
  await screen.findByRole(path.startsWith("/session") ? "toolbar" : "navigation");
  return { router, queryClient, user };
}
