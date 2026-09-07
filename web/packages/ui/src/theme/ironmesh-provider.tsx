import { MantineProvider, localStorageColorSchemeManager } from "@mantine/core";
import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode
} from "react";
import {
  buildIronmeshAccentCssVariables,
  createIronmeshTheme,
  defaultIronmeshAccentColor,
  defaultIronmeshAccentCssVariables,
  ironmeshAccentColorStorageKey,
  normalizeIronmeshAccentColor,
  readIronmeshHostAccentColor,
  type IronmeshEmbeddedClient
} from "./ironmesh-theme";

export const ironmeshColorSchemeStorageKey = "ironmesh-color-scheme";

const ironmeshColorSchemeManager = localStorageColorSchemeManager({
  key: ironmeshColorSchemeStorageKey
});

type IronmeshMantineProviderProps = {
  children: ReactNode;
};

type IronmeshAccentColorContextValue = {
  accentColor: string;
  setAccentColor: (value: string) => void;
  resetAccentColor: () => void;
  accentColorHost: IronmeshEmbeddedClient | null;
};

const IronmeshAccentColorContext = createContext<IronmeshAccentColorContextValue | null>(null);

export function IronmeshMantineProvider({ children }: IronmeshMantineProviderProps) {
  const hostAccent = useMemo(() => readIronmeshHostAccentColor(), []);
  const accentColorHost = hostAccent?.client ?? null;
  const [accentColor, setAccentColorState] = useState(
    () => hostAccent?.color ?? readStoredAccentColor()
  );
  const theme = useMemo(() => createIronmeshTheme(accentColor), [accentColor]);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    if (hostAccent) {
      setAccentColorState(hostAccent.color);
      return;
    }

    try {
      if (accentColor === defaultIronmeshAccentColor) {
        window.localStorage.removeItem(ironmeshAccentColorStorageKey);
      } else {
        window.localStorage.setItem(ironmeshAccentColorStorageKey, accentColor);
      }
    } catch {
      // Ignore local persistence failures and keep the active in-memory theme.
    }
  }, [accentColor, hostAccent]);

  useEffect(() => {
    if (typeof document === "undefined") {
      return;
    }

    applyAccentCssVariables(document.documentElement, accentColor);
  }, [accentColor]);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    if (hostAccent) {
      return;
    }

    function handleStorage(event: StorageEvent) {
      if (event.storageArea !== window.localStorage || event.key !== ironmeshAccentColorStorageKey) {
        return;
      }

      setAccentColorState(readStoredAccentColor());
    }

    window.addEventListener("storage", handleStorage);
    return () => window.removeEventListener("storage", handleStorage);
  }, [hostAccent]);

  const accentColorContextValue = useMemo<IronmeshAccentColorContextValue>(
    () => ({
      accentColor,
      setAccentColor(value: string) {
        if (accentColorHost) {
          return;
        }

        const normalized = normalizeIronmeshAccentColor(value);
        if (normalized) {
          setAccentColorState(normalized);
        }
      },
      resetAccentColor() {
        if (accentColorHost) {
          return;
        }

        setAccentColorState(defaultIronmeshAccentColor);
      },
      accentColorHost
    }),
    [accentColor, accentColorHost]
  );

  return (
    <IronmeshAccentColorContext.Provider value={accentColorContextValue}>
      <MantineProvider
        theme={theme}
        colorSchemeManager={ironmeshColorSchemeManager}
        defaultColorScheme="auto"
      >
        {children}
      </MantineProvider>
    </IronmeshAccentColorContext.Provider>
  );
}

export function useIronmeshAccentColor() {
  const context = useContext(IronmeshAccentColorContext);
  if (!context) {
    throw new Error("useIronmeshAccentColor must be used inside IronmeshMantineProvider");
  }

  return context;
}

function readStoredAccentColor() {
  if (typeof window === "undefined") {
    return defaultIronmeshAccentColor;
  }

  try {
    return normalizeIronmeshAccentColor(window.localStorage.getItem(ironmeshAccentColorStorageKey))
      ?? defaultIronmeshAccentColor;
  } catch {
    return defaultIronmeshAccentColor;
  }
}

function applyAccentCssVariables(target: HTMLElement, accentColor: string) {
  const accentVariables = buildIronmeshAccentCssVariables(accentColor);

  for (const [name, fallback] of Object.entries(defaultIronmeshAccentCssVariables)) {
    target.style.setProperty(name, accentVariables[name] ?? fallback);
  }
}
