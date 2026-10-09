import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App, createQueryClient } from "./app";
import "./index.css";
import { createAppRouter } from "./router";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App router={createAppRouter()} queryClient={createQueryClient()} />
  </StrictMode>,
);
