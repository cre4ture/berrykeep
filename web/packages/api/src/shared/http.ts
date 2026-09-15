export class HttpError extends Error {
  readonly status: number;
  readonly payload: unknown;

  constructor(status: number, payload: unknown, messagePayload = payload) {
    super(
      `HTTP ${status}: ${JSON.stringify(messagePayload ?? { message: "no JSON body returned" })}`
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
  if (response.ok) {
    return (await response.json().catch(() => null)) as T;
  }

  const text = await response.text();
  let payload: unknown = null;
  let isPlainTextError = false;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      // Compatibility probes need this detail, but it must not become a
      // user-visible error message for arbitrary upstream responses.
      isPlainTextError = true;
      payload =
        text.length > NON_JSON_ERROR_PAYLOAD_MAX_LENGTH
          ? `${text.slice(0, NON_JSON_ERROR_PAYLOAD_MAX_LENGTH)}…`
          : text;
    }
  }

  throw new HttpError(response.status, payload, isPlainTextError ? null : payload);
}
