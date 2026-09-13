import { BerryKeepQueryProvider } from "@berrykeep/ui";
import { ClientShell } from "./app-shell/ClientShell";

export function App() {
  return (
    <BerryKeepQueryProvider>
      <ClientShell />
    </BerryKeepQueryProvider>
  );
}
