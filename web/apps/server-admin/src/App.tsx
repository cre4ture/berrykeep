import { BerryKeepQueryProvider } from "@berrykeep/ui";
import { ServerAdminShell } from "./app-shell/ServerAdminShell";
import { AdminAccessProvider } from "./lib/admin-access";

export function App() {
  return (
    <BerryKeepQueryProvider>
      <AdminAccessProvider>
        <ServerAdminShell />
      </AdminAccessProvider>
    </BerryKeepQueryProvider>
  );
}
