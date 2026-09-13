export class HttpError extends Error {
  readonly status: number;
  readonly payload: unknown;

  constructor(status: number, payload: unknown) {
    super(
      `HTTP ${status}: ${JSON.stringify(payload ?? { message: "no JSON body returned" })}`
    );
    this.name = "HttpError";
    this.status = status;
    this.payload = payload;
  }
}

const NON_JSON_ERROR_PAYLOAD_MAX_LENGTH = 512;

export function isHttpErrorStatus(
  error: unknown,
  ...statuses: number[]
): boolean {
  return error instanceof HttpError && statuses.includes(error.status);
}

export async function fetchJson<T>(
  input: RequestInfo | URL,
  init?: RequestInit
): Promise<T> {
  const response = await fetch(input, init);
  const text = await response.text();
  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      // Preserve enough plain-text detail for narrow compatibility probes,
      // without rendering a complete proxy error page in a UI error banner.
      payload =
        text.length > NON_JSON_ERROR_PAYLOAD_MAX_LENGTH
          ? `${text.slice(0, NON_JSON_ERROR_PAYLOAD_MAX_LENGTH)}…`
          : text;
    }
  }

  if (!response.ok) {
    throw new HttpError(response.status, payload);
  }

  return payload as T;
}
