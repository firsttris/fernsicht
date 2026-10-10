import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createMemoryHistory } from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { createViewerRouter } from "./router";

/** Renders the viewer at `path` with in-memory history. */
export async function renderViewer(
  path: string,
  waitFor: string = path.startsWith("/session") ? "toolbar" : "form",
) {
  const router = createViewerRouter(createMemoryHistory({ initialEntries: [path] }));
  render(
    <QueryClientProvider client={new QueryClient()}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await screen.findByRole(waitFor);
  return { router, user: userEvent.setup() };
}
