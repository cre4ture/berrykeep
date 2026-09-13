import "@mantine/core/styles.css";
import "./styles/globals.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BerryKeepMantineProvider } from "@berrykeep/ui";
import { App } from "./App";

const root = document.getElementById("root");

if (!root) {
  throw new Error("root element is missing");
}

createRoot(root).render(
  <StrictMode>
    <BerryKeepMantineProvider>
      <App />
    </BerryKeepMantineProvider>
  </StrictMode>
);
