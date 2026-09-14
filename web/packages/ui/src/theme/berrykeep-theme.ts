import { createTheme, type MantineColorsTuple } from "@mantine/core";

export const berrykeepPrimaryColor = "brand";
export const defaultBerryKeepAccentColor = "#14b8a6";
export const berrykeepAccentColorStorageKey = "berrykeep-accent-color";
export const berrykeepAccentColorQueryParameter = "accent_color";
export const berrykeepEmbeddedClientQueryParameter = "embedded_client";

export type BerryKeepEmbeddedClient = "android" | "ios";

type RgbColor = {
  r: number;
  g: number;
  b: number;
};

const white: RgbColor = { r: 255, g: 255, b: 255 };
const black: RgbColor = { r: 0, g: 0, b: 0 };

export const berrykeepTheme = createBerryKeepTheme();

export const defaultBerryKeepAccentCssVariables = buildBerryKeepAccentCssVariables(
  defaultBerryKeepAccentColor
);

export function normalizeBerryKeepAccentColor(value: string | null | undefined): string | null {
  const trimmed = value?.trim();
  if (!trimmed) {
    return null;
  }

  const shortHexMatch = /^#?([\da-f]{3})$/i.exec(trimmed);
  if (shortHexMatch) {
    return `#${shortHexMatch[1]
      .split("")
      .map((channel) => `${channel}${channel}`)
      .join("")
      .toLowerCase()}`;
  }

  const longHexMatch = /^#?([\da-f]{6})$/i.exec(trimmed);
  if (longHexMatch) {
    return `#${longHexMatch[1].toLowerCase()}`;
  }

  return null;
}

/**
 * Returns the native host's accent color for embedded Android and iOS Web UIs.
 *
 * A native host explicitly opts into host-managed color with `embedded_client`.
 * This URL contract is also useful for browser-based previews and automated tests;
 * it is not an authenticity signal for a native WebView.
 */
export function readBerryKeepHostAccentColor(
  search = typeof window === "undefined" ? "" : window.location.search
): { client: BerryKeepEmbeddedClient; color: string } | null {
  const parameters = new URLSearchParams(search);
  const client = parameters.get(berrykeepEmbeddedClientQueryParameter);
  if (client !== "android" && client !== "ios") {
    return null;
  }

  const color = normalizeBerryKeepAccentColor(
    parameters.get(berrykeepAccentColorQueryParameter)
  );
  return color ? { client, color } : null;
}

export function createBerryKeepTheme(accentColor = defaultBerryKeepAccentColor) {
  const colors = {
    [berrykeepPrimaryColor]: buildBerryKeepColorScale(accentColor)
  } satisfies Record<typeof berrykeepPrimaryColor, MantineColorsTuple>;

  return createTheme({
    colors,
    primaryColor: berrykeepPrimaryColor,
    fontFamily: "Space Grotesk, system-ui, sans-serif",
    headings: {
      fontFamily: "Space Grotesk, system-ui, sans-serif"
    }
  });
}

function buildBerryKeepColorScale(accentColor: string): MantineColorsTuple {
  const normalized = normalizeBerryKeepAccentColor(accentColor) ?? defaultBerryKeepAccentColor;
  const rgb = parseHexColor(normalized);

  return [
    mixColors(rgb, white, 0.92),
    mixColors(rgb, white, 0.82),
    mixColors(rgb, white, 0.68),
    mixColors(rgb, white, 0.52),
    mixColors(rgb, white, 0.35),
    mixColors(rgb, white, 0.18),
    normalized,
    mixColors(rgb, black, 0.12),
    mixColors(rgb, black, 0.24),
    mixColors(rgb, black, 0.38)
  ];
}

function parseHexColor(color: string): RgbColor {
  const normalized = normalizeBerryKeepAccentColor(color) ?? defaultBerryKeepAccentColor;
  const value = normalized.slice(1);

  return {
    r: Number.parseInt(value.slice(0, 2), 16),
    g: Number.parseInt(value.slice(2, 4), 16),
    b: Number.parseInt(value.slice(4, 6), 16)
  };
}

export function buildBerryKeepAccentCssVariables(accentColor: string): Record<string, string> {
  const scale = buildBerryKeepColorScale(accentColor);

  return {
    "--berrykeep-accent-rgb": toRgbChannels(parseHexColor(scale[6])),
    "--berrykeep-accent-soft-rgb": toRgbChannels(parseHexColor(scale[3])),
    "--berrykeep-accent-deep-rgb": toRgbChannels(parseHexColor(scale[8])),
    "--berrykeep-accent-strong-rgb": toRgbChannels(parseHexColor(scale[9]))
  };
}

function mixColors(color: RgbColor, target: RgbColor, amount: number): string {
  return rgbToHex({
    r: mixChannel(color.r, target.r, amount),
    g: mixChannel(color.g, target.g, amount),
    b: mixChannel(color.b, target.b, amount)
  });
}

function mixChannel(value: number, target: number, amount: number) {
  return Math.round(value + (target - value) * amount);
}

function rgbToHex(color: RgbColor): string {
  return `#${toHexChannel(color.r)}${toHexChannel(color.g)}${toHexChannel(color.b)}`;
}

function toHexChannel(value: number): string {
  return Math.max(0, Math.min(255, value)).toString(16).padStart(2, "0");
}

function toRgbChannels(color: RgbColor): string {
  return `${color.r}, ${color.g}, ${color.b}`;
}
