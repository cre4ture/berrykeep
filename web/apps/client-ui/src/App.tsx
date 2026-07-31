import { IronmeshQueryProvider } from "@ironmesh/ui";
import { ClientShell } from "./app-shell/ClientShell";

export function App() {
  return (
    <IronmeshQueryProvider>
      <ClientShell />
    </IronmeshQueryProvider>
  );
}
