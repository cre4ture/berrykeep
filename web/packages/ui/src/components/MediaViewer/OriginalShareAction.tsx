import { Button } from "@mantine/core";
import { IconShare } from "@tabler/icons-react";
import { useEffect, useState } from "react";

export type MediaShareRequest = {
  key: string;
  snapshotId?: string | null;
  versionId?: string | null;
  fileName: string;
  mimeType?: string | null;
  sizeBytes?: number | null;
};

export type EmbeddedShareClient = "android" | "ios";

type AndroidShareBridgeMessage = {
  data: unknown;
};

type AndroidShareBridge = {
  postMessage: (message: string) => void;
  addEventListener: (
    type: "message",
    listener: (message: AndroidShareBridgeMessage) => void
  ) => void;
  removeEventListener: (
    type: "message",
    listener: (message: AndroidShareBridgeMessage) => void
  ) => void;
};

type IosShareBridge = {
  postMessage: (message: Record<string, unknown>) => Promise<unknown>;
};

type OriginalShareResponse = {
  requestId: string;
  status: "opened" | "error";
  message?: string;
};

type OriginalShareActionProps = {
  client: EmbeddedShareClient;
  request: MediaShareRequest | null | undefined;
};

type ShareStatus =
  | { state: "idle" }
  | { state: "pending" }
  | { state: "opened" }
  | { state: "error"; message: string };

declare global {
  interface Window {
    IronmeshAndroidShare?: AndroidShareBridge;
    webkit?: {
      messageHandlers?: {
        IronmeshIosShare?: IosShareBridge;
      };
    };
  }
}

const SHARE_RESPONSE_TIMEOUT_MS = 10_000;

export function OriginalShareAction({ client, request }: OriginalShareActionProps) {
  const [status, setStatus] = useState<ShareStatus>({ state: "idle" });
  const bridgeAvailable = hasShareBridge(client);
  const platformName = client === "android" ? "Android" : "iOS";

  useEffect(() => {
    setStatus({ state: "idle" });
  }, [request?.key, request?.snapshotId, request?.versionId]);

  useEffect(() => {
    if (status.state !== "opened") {
      return;
    }
    const timeout = window.setTimeout(() => setStatus({ state: "idle" }), 3_000);
    return () => window.clearTimeout(timeout);
  }, [status.state]);

  const unavailableReason = !request
    ? "This media item has no immutable snapshot or version selector"
    : !bridgeAvailable
      ? `The ${platformName} share bridge is unavailable in this WebView`
      : null;
  const title =
    status.state === "error"
      ? status.message
      : unavailableReason ?? `Share the original media file through ${platformName}`;
  const label =
    status.state === "pending"
      ? "Preparing share…"
      : status.state === "opened"
        ? "Share opened"
        : status.state === "error"
          ? "Share failed"
          : "Share original";

  async function shareOriginal() {
    if (!request || !bridgeAvailable || status.state === "pending") {
      return;
    }

    const requestId = createShareRequestId();
    setStatus({ state: "pending" });
    try {
      const responsePromise = sendShareRequest(client, {
        action: "share-original",
        requestId,
        ...request
      });
      const response = await (client === "ios"
        ? withTimeout(
            responsePromise,
            SHARE_RESPONSE_TIMEOUT_MS,
            `${platformName} did not respond to the share request.`
          )
        : responsePromise);
      const parsed = parseShareResponse(response);
      if (!parsed || parsed.requestId !== requestId) {
        throw new Error(`${platformName} returned an invalid share response.`);
      }
      setStatus(
        parsed.status === "opened"
          ? { state: "opened" }
          : {
              state: "error",
              message: parsed.message || `${platformName} could not share this file.`
            }
      );
    } catch (error) {
      setStatus({
        state: "error",
        message: error instanceof Error ? error.message : `${platformName} could not share this file.`
      });
    }
  }

  return (
    <Button
      data-media-share="true"
      variant="default"
      size="xs"
      leftSection={<IconShare size={14} />}
      disabled={Boolean(unavailableReason)}
      loading={status.state === "pending"}
      color={status.state === "error" ? "red" : undefined}
      title={title}
      aria-live="polite"
      onClick={shareOriginal}
    >
      {label}
    </Button>
  );
}

export function embeddedShareClient(): EmbeddedShareClient | null {
  if (typeof window === "undefined") {
    return null;
  }
  const client = new URLSearchParams(window.location.search).get("embedded_client");
  return client === "android" || client === "ios" ? client : null;
}

function hasShareBridge(client: EmbeddedShareClient): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return client === "android"
    ? Boolean(window.IronmeshAndroidShare)
    : Boolean(window.webkit?.messageHandlers?.IronmeshIosShare);
}

function sendShareRequest(
  client: EmbeddedShareClient,
  payload: Record<string, unknown>
): Promise<unknown> {
  if (client === "ios") {
    const bridge = window.webkit?.messageHandlers?.IronmeshIosShare;
    if (!bridge) {
      return Promise.reject(new Error("The iOS share bridge is unavailable in this WebView."));
    }
    return bridge.postMessage(payload);
  }

  const bridge = window.IronmeshAndroidShare;
  if (!bridge) {
    return Promise.reject(new Error("The Android share bridge is unavailable in this WebView."));
  }
  const androidBridge = bridge;

  return new Promise((resolve, reject) => {
    const requestId = String(payload.requestId ?? "");
    const timeout = window.setTimeout(() => {
      androidBridge.removeEventListener("message", handleMessage);
      reject(new Error("Android did not respond to the share request."));
    }, SHARE_RESPONSE_TIMEOUT_MS);
    function handleMessage(message: AndroidShareBridgeMessage) {
      const response = parseShareResponse(message.data);
      if (!response || response.requestId !== requestId) {
        return;
      }
      window.clearTimeout(timeout);
      androidBridge.removeEventListener("message", handleMessage);
      resolve(message.data);
    }

    androidBridge.addEventListener("message", handleMessage);
    try {
      androidBridge.postMessage(JSON.stringify(payload));
    } catch (error) {
      window.clearTimeout(timeout);
      androidBridge.removeEventListener("message", handleMessage);
      reject(error);
    }
  });
}

function parseShareResponse(value: unknown): OriginalShareResponse | null {
  try {
    const candidate = (typeof value === "string" ? JSON.parse(value) : value) as
      | Partial<OriginalShareResponse>
      | null;
    if (
      !candidate ||
      typeof candidate.requestId !== "string" ||
      (candidate.status !== "opened" && candidate.status !== "error")
    ) {
      return null;
    }
    return {
      requestId: candidate.requestId,
      status: candidate.status,
      message: typeof candidate.message === "string" ? candidate.message : undefined
    };
  } catch {
    return null;
  }
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, message: string): Promise<T> {
  return new Promise((resolve, reject) => {
    const timeout = window.setTimeout(() => reject(new Error(message)), timeoutMs);
    promise.then(
      (value) => {
        window.clearTimeout(timeout);
        resolve(value);
      },
      (error) => {
        window.clearTimeout(timeout);
        reject(error);
      }
    );
  });
}

function createShareRequestId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `share-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
