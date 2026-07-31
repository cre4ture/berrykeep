import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";

type IronmeshQueryProviderProps = {
  children: ReactNode;
};

/**
 * Creates an application-local query cache with the shared IronMesh defaults.
 *
 * Every application mounts its own provider, so data and credentials are never
 * shared across the client and server-admin browser applications.
 */
export function IronmeshQueryProvider({ children }: IronmeshQueryProviderProps) {
  const [queryClient] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: {
            retry: false,
            refetchOnWindowFocus: false
          },
          mutations: {
            retry: false
          }
        }
      })
  );

  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
}
