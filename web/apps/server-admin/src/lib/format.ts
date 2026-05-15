const relativeTimeFormatter = new Intl.RelativeTimeFormat("en", {
  numeric: "auto"
});

const RELATIVE_TIME_UNITS: Array<{
  maxSeconds: number;
  secondsPerUnit: number;
  unit: Intl.RelativeTimeFormatUnit;
}> = [
  { maxSeconds: 60, secondsPerUnit: 1, unit: "second" },
  { maxSeconds: 60 * 60, secondsPerUnit: 60, unit: "minute" },
  { maxSeconds: 60 * 60 * 24, secondsPerUnit: 60 * 60, unit: "hour" },
  { maxSeconds: 60 * 60 * 24 * 7, secondsPerUnit: 60 * 60 * 24, unit: "day" },
  { maxSeconds: 60 * 60 * 24 * 30, secondsPerUnit: 60 * 60 * 24 * 7, unit: "week" },
  {
    maxSeconds: 60 * 60 * 24 * 365,
    secondsPerUnit: 60 * 60 * 24 * 30,
    unit: "month"
  },
  {
    maxSeconds: Number.POSITIVE_INFINITY,
    secondsPerUnit: 60 * 60 * 24 * 365,
    unit: "year"
  }
];

export function formatUnixTs(unixTs?: number | null): string {
  if (!unixTs || !Number.isFinite(unixTs) || unixTs <= 0) {
    return "unknown";
  }
  return new Date(unixTs * 1000).toISOString();
}

export function formatRelativeUnixTs(unixTs?: number | null, nowMs = Date.now()): string {
  if (!unixTs || !Number.isFinite(unixTs) || unixTs <= 0 || !Number.isFinite(nowMs)) {
    return "unknown";
  }

  const deltaSeconds = unixTs - nowMs / 1000;
  for (const { maxSeconds, secondsPerUnit, unit } of RELATIVE_TIME_UNITS) {
    if (Math.abs(deltaSeconds) < maxSeconds) {
      const roundedDelta =
        deltaSeconds < 0
          ? Math.ceil(deltaSeconds / secondsPerUnit)
          : Math.floor(deltaSeconds / secondsPerUnit);
      return relativeTimeFormatter.format(roundedDelta, unit);
    }
  }

  return "unknown";
}

export function formatBytes(bytes?: number | null): string {
  if (bytes == null || !Number.isFinite(bytes)) {
    return "unknown";
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unitIndex = -1;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unitIndex]}`;
}
