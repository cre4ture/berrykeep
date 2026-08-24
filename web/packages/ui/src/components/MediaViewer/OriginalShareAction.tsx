import { Button } from "@mantine/core";
import { IconShare } from "@tabler/icons-react";
import { useEffect, useRef, useState } from "react";

export type MediaShareRequest = {
  key: string;
  snapshotId?: string | null;
  versionId?: string | null;
  fileName: string;
  mimeType?: string | null;
  sizeBytes?: number | null;
};

export type MediaShareVersionResolver = (key: string) => Promise<string | null>;

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
  resolveVersionId?: MediaShareVersionResolver;
};

type ShareStatus =
  | { state: "idle" }
  | { state: "pending" }
  | { state: "opened" }
  | { state: "error"; message: string };

type ScopedShareStatus = {
  requestIdentity: string;
  value: ShareStatus;
};

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

export function OriginalShareAction({
  client,
  request,
  resolveVersionId
}: OriginalShareActionProps) {
  const requestIdentity = JSON.stringify([
    client,
    request?.key,
    request?.snapshotId,
    request?.versionId
  ]);
  const [scopedStatus, setScopedStatus] = useState<ScopedShareStatus>({
    requestIdentity,
    value: { state: "idle" }
  });
  const activeRequest = useRef<{ requestId: string; requestIdentity: string } | null>(null);
  if (activeRequest.current?.requestIdentity !== requestIdentity) {
    activeRequest.current = null;
  }
  const status: ShareStatus =
    scopedStatus.requestIdentity === requestIdentity
      ? scopedStatus.value
      : { state: "idle" };
  const bridgeAvailable = hasShareBridge(client);
  const platformName = client === "android" ? "Android" : "iOS";

  useEffect(() => {
    if (status.state !== "opened") {
      return;
    }
    const timeout = window.setTimeout(
      () =>
        setScopedStatus((current) =>
          current.requestIdentity === requestIdentity
            ? { requestIdentity, value: { state: "idle" } }
            : current
        ),
      3_000
    );
    return () => window.clearTimeout(timeout);
  }, [requestIdentity, status.state]);

  const selectorState = mediaShareSelectorState(request);
  const unavailableReason = !request
    ? "This media item has no original-share request"
    : selectorState === "invalid"
      ? "This media item has conflicting snapshot and version selectors"
      : selectorState === "unresolved" && !resolveVersionId
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
    if (
      !request ||
      !bridgeAvailable ||
      status.state === "pending" ||
      activeRequest.current !== null
    ) {
      return;
    }

    const requestId = createShareRequestId();
    activeRequest.current = { requestId, requestIdentity };
    setScopedStatus({ requestIdentity, value: { state: "pending" } });
    try {
      const resolvedRequest = await resolveImmutableShareRequest(request, resolveVersionId);
      if (
        activeRequest.current?.requestId !== requestId ||
        activeRequest.current.requestIdentity !== requestIdentity
      ) {
        return;
      }
      const responsePromise = sendShareRequest(client, {
        action: "share-original",
        requestId,
        ...resolvedRequest
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
      if (
        activeRequest.current?.requestId !== requestId ||
        activeRequest.current.requestIdentity !== requestIdentity
      ) {
        return;
      }
      activeRequest.current = null;
      setScopedStatus({
        requestIdentity,
        value:
          parsed.status === "opened"
            ? { state: "opened" }
            : {
                state: "error",
                message: parsed.message || `${platformName} could not share this file.`
              }
      });
    } catch (error) {
      if (
        activeRequest.current?.requestId !== requestId ||
        activeRequest.current.requestIdentity !== requestIdentity
      ) {
        return;
      }
      activeRequest.current = null;
      setScopedStatus({
        requestIdentity,
        value: {
          state: "error",
          message:
            error instanceof Error ? error.message : `${platformName} could not share this file.`
        }
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

type MediaShareSelectorState = "resolved" | "unresolved" | "invalid";

function mediaShareSelectorState(
  request: MediaShareRequest | null | undefined
): MediaShareSelectorState {
  if (!request) {
    return "unresolved";
  }

  const hasSnapshot = Boolean(request.snapshotId?.trim());
  const hasVersion = Boolean(request.versionId?.trim());
  if (hasSnapshot && hasVersion) {
    return "invalid";
  }
  return hasSnapshot || hasVersion ? "resolved" : "unresolved";
}

async function resolveImmutableShareRequest(
  request: MediaShareRequest,
  resolveVersionId: MediaShareVersionResolver | undefined
): Promise<MediaShareRequest> {
  const snapshotId = request.snapshotId?.trim() || null;
  const versionId = request.versionId?.trim() || null;
  if (snapshotId && versionId) {
    throw new Error("The original-share request has conflicting snapshot and version selectors.");
  }
  if (snapshotId || versionId) {
    return { ...request, snapshotId, versionId };
  }
  if (!resolveVersionId) {
    throw new Error("No immutable version is available for this media item.");
  }

  const resolvedVersionId = (await resolveVersionId(request.key))?.trim() || null;
  if (!resolvedVersionId) {
    throw new Error("The current media item has no preferred version to share.");
  }
  return {
    ...request,
    snapshotId: null,
    versionId: resolvedVersionId
  };
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
