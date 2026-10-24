import type { Page, Route } from "@playwright/test";

/** Keep matching responses pending while an action observes the loading state. */
export async function withPausedResponse(
  page: Page,
  url: Parameters<Page["route"]>[0],
  action: () => Promise<void>
): Promise<void> {
  let release!: () => void;
  const responseAllowed = new Promise<void>((resolve) => {
    release = resolve;
  });
  const handler = async (route: Route) => {
    await responseAllowed;
    await route.fallback();
  };
  await page.route(url, handler);
  try {
    await action();
  } finally {
    release();
    await page.unroute(url, handler);
  }
}
