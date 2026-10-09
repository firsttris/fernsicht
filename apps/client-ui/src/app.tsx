import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";

import type { createAppRouter } from "./router";

export function createQueryClient() {
  return new QueryClient({ defaultOptions: { queries: { staleTime: 10_000 } } });
}

export function App({
  router,
  queryClient,
}: {
  router: ReturnType<typeof createAppRouter>;
  queryClient: QueryClient;
}) {
  return (
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  );
}
