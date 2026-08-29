import "@mantine/core/styles.css";
import "./styles/globals.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { IronmeshMantineProvider, IronmeshQueryProvider } from "@ironmesh/ui/fleet-telemetry";
import { App } from "./App";

const root = document.getElementById("root");

if (!root) {
  throw new Error("root element is missing");
}

createRoot(root).render(
  <StrictMode>
    <IronmeshMantineProvider>
      <IronmeshQueryProvider>
        <App />
      </IronmeshQueryProvider>
    </IronmeshMantineProvider>
  </StrictMode>
);
