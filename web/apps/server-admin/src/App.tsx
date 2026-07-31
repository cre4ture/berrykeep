import { IronmeshQueryProvider } from "@ironmesh/ui";
import { ServerAdminShell } from "./app-shell/ServerAdminShell";
import { AdminAccessProvider } from "./lib/admin-access";

export function App() {
  return (
    <IronmeshQueryProvider>
      <AdminAccessProvider>
        <ServerAdminShell />
      </AdminAccessProvider>
    </IronmeshQueryProvider>
  );
}
