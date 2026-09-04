export type GalleryCaptureDateBounds = {
  capturedFromUnix?: number;
  capturedUntilUnix?: number;
};

/**
 * Converts browser date-input values into local-calendar Unix bounds.
 * The upper bound is the start of the next local day so the displayed
 * "through" date remains inclusive across daylight-saving transitions.
 */
export function galleryCaptureDateBounds(
  fromDate: string,
  throughDate: string
): GalleryCaptureDateBounds {
  const from = localDateStart(fromDate);
  const through = localDateStart(throughDate);
  return {
    ...(from === null ? {} : { capturedFromUnix: unixSeconds(from) }),
    ...(through === null
      ? {}
      : {
          capturedUntilUnix: unixSeconds(
            new Date(through.getFullYear(), through.getMonth(), through.getDate() + 1)
          )
        })
  };
}

function localDateStart(value: string): Date | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!match) {
    return null;
  }
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const date = new Date(year, month - 1, day);
  if (
    date.getFullYear() !== year ||
    date.getMonth() !== month - 1 ||
    date.getDate() !== day
  ) {
    return null;
  }
  return date;
}

function unixSeconds(date: Date): number {
  return Math.floor(date.getTime() / 1_000);
}
