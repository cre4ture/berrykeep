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
  buildBerryKeepAccentCssVariables,
  createBerryKeepTheme,
  defaultBerryKeepAccentColor,
  defaultBerryKeepAccentCssVariables,
  berrykeepAccentColorStorageKey,
  normalizeBerryKeepAccentColor,
  readBerryKeepHostAccentColor,
  type BerryKeepEmbeddedClient
} from "./berrykeep-theme";

export const berrykeepColorSchemeStorageKey = "berrykeep-color-scheme";

const berrykeepColorSchemeManager = localStorageColorSchemeManager({
  key: berrykeepColorSchemeStorageKey
});

type BerryKeepMantineProviderProps = {
  children: ReactNode;
};

type BerryKeepAccentColorContextValue = {
  accentColor: string;
  setAccentColor: (value: string) => void;
  resetAccentColor: () => void;
  accentColorHost: BerryKeepEmbeddedClient | null;
};

const BerryKeepAccentColorContext = createContext<BerryKeepAccentColorContextValue | null>(null);

export function BerryKeepMantineProvider({ children }: BerryKeepMantineProviderProps) {
  const hostAccent = useMemo(() => readBerryKeepHostAccentColor(), []);
  const accentColorHost = hostAccent?.client ?? null;
  const [accentColor, setAccentColorState] = useState(
    () => hostAccent?.color ?? readStoredAccentColor()
  );
  const theme = useMemo(() => createBerryKeepTheme(accentColor), [accentColor]);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    if (hostAccent) {
      setAccentColorState(hostAccent.color);
      return;
    }

    try {
      if (accentColor === defaultBerryKeepAccentColor) {
        window.localStorage.removeItem(berrykeepAccentColorStorageKey);
      } else {
        window.localStorage.setItem(berrykeepAccentColorStorageKey, accentColor);
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
      if (event.storageArea !== window.localStorage || event.key !== berrykeepAccentColorStorageKey) {
        return;
      }

      setAccentColorState(readStoredAccentColor());
    }

    window.addEventListener("storage", handleStorage);
    return () => window.removeEventListener("storage", handleStorage);
  }, [hostAccent]);

  const accentColorContextValue = useMemo<BerryKeepAccentColorContextValue>(
    () => ({
      accentColor,
      setAccentColor(value: string) {
        if (accentColorHost) {
          return;
        }

        const normalized = normalizeBerryKeepAccentColor(value);
        if (normalized) {
          setAccentColorState(normalized);
        }
      },
      resetAccentColor() {
        if (accentColorHost) {
          return;
        }

        setAccentColorState(defaultBerryKeepAccentColor);
      },
      accentColorHost
    }),
    [accentColor, accentColorHost]
  );

  return (
    <BerryKeepAccentColorContext.Provider value={accentColorContextValue}>
      <MantineProvider
        theme={theme}
        colorSchemeManager={berrykeepColorSchemeManager}
        defaultColorScheme="auto"
      >
        {children}
      </MantineProvider>
    </BerryKeepAccentColorContext.Provider>
  );
}

export function useBerryKeepAccentColor() {
  const context = useContext(BerryKeepAccentColorContext);
  if (!context) {
    throw new Error("useBerryKeepAccentColor must be used inside BerryKeepMantineProvider");
  }

  return context;
}

function readStoredAccentColor() {
  if (typeof window === "undefined") {
    return defaultBerryKeepAccentColor;
  }

  try {
    return normalizeBerryKeepAccentColor(window.localStorage.getItem(berrykeepAccentColorStorageKey))
      ?? defaultBerryKeepAccentColor;
  } catch {
    return defaultBerryKeepAccentColor;
  }
}

function applyAccentCssVariables(target: HTMLElement, accentColor: string) {
  const accentVariables = buildBerryKeepAccentCssVariables(accentColor);

  for (const [name, fallback] of Object.entries(defaultBerryKeepAccentCssVariables)) {
    target.style.setProperty(name, accentVariables[name] ?? fallback);
  }
}
