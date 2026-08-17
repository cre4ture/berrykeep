import { Button } from "@mantine/core";
import { IconShare } from "@tabler/icons-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

export type MediaShareRequest = {
  key: string;
  snapshotId?: string | null;
  versionId?: string | null;
  fileName: string;
  mimeType?: string | null;
  sizeBytes?: number | null;
};

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

type AndroidShareResponse = {
  requestId: string;
  status: "opened" | "error";
  message?: string;
};

type AndroidOriginalShareActionProps = {
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
  }
}

export function AndroidOriginalShareAction({ request }: AndroidOriginalShareActionProps) {
  const [status, setStatus] = useState<ShareStatus>({ state: "idle" });
  const pendingRequestId = useRef<string | null>(null);
  const bridge = typeof window === "undefined" ? undefined : window.IronmeshAndroidShare;

  useEffect(() => {
    setStatus({ state: "idle" });
    pendingRequestId.current = null;
  }, [request?.key, request?.snapshotId, request?.versionId]);

  useLayoutEffect(() => {
    if (!bridge) {
      return;
    }

    function handleMessage(message: AndroidShareBridgeMessage) {
      const response = parseAndroidShareResponse(message.data);
      if (!response || response.requestId !== pendingRequestId.current) {
        return;
      }
      pendingRequestId.current = null;
      setStatus(
        response.status === "opened"
          ? { state: "opened" }
          : { state: "error", message: response.message || "Android could not share this file." }
      );
    }

    bridge.addEventListener("message", handleMessage);
    return () => bridge.removeEventListener("message", handleMessage);
  }, [bridge]);

  useEffect(() => {
    if (status.state !== "opened") {
      return;
    }
    const timeout = window.setTimeout(() => setStatus({ state: "idle" }), 3_000);
    return () => window.clearTimeout(timeout);
  }, [status.state]);

  useEffect(() => {
    if (status.state !== "pending") {
      return;
    }
    const timeout = window.setTimeout(() => {
      pendingRequestId.current = null;
      setStatus({ state: "error", message: "Android did not respond to the share request." });
    }, 10_000);
    return () => window.clearTimeout(timeout);
  }, [status.state]);

  const unavailableReason = !request
    ? "This media item has no immutable snapshot or version selector"
    : !bridge
      ? "The Android share bridge is unavailable in this WebView"
      : null;
  const title =
    status.state === "error"
      ? status.message
      : unavailableReason ?? "Share the original media file through Android";
  const label =
    status.state === "pending"
      ? "Preparing share…"
      : status.state === "opened"
        ? "Share opened"
        : status.state === "error"
          ? "Share failed"
          : "Share original";

  function shareOriginal() {
    if (!request || !bridge || status.state === "pending") {
      return;
    }

    const requestId = createShareRequestId();
    pendingRequestId.current = requestId;
    setStatus({ state: "pending" });
    try {
      bridge.postMessage(
        JSON.stringify({
          action: "share-original",
          requestId,
          ...request
        })
      );
    } catch (error) {
      pendingRequestId.current = null;
      setStatus({
        state: "error",
        message: error instanceof Error ? error.message : "Android could not share this file."
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

export function usesAndroidEmbeddedClient(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return new URLSearchParams(window.location.search).get("embedded_client") === "android";
}

function parseAndroidShareResponse(value: unknown): AndroidShareResponse | null {
  try {
    const candidate = JSON.parse(String(value)) as Partial<AndroidShareResponse>;
    if (
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

function createShareRequestId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `share-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
