import { Modal, type ModalProps } from "@mantine/core";
import type { CSSProperties } from "react";

type EmbeddedViewportModalProps = Omit<ModalProps, "styles"> & {
  /**
   * Android WebView can report a zero dynamic viewport height while a fixed
   * fullscreen surface is active. Mantine's default modal height uses dvh.
   */
  usesEmbeddedViewport?: boolean;
};

const embeddedViewportModalStyles = {
  root: {
    "--modal-y-offset": "16px",
    "--modal-x-offset": "16px"
  } as CSSProperties,
  // The modal inner container is fixed with top and bottom bounds, so this
  // percentage is resolved from the actual WebView bounds instead of dvh.
  content: {
    maxHeight: "calc(100% - 32px)"
  }
};

/**
 * A viewport-safe Mantine modal for embedded WebViews. Keep viewport sizing in
 * this adapter so fullscreen content, dialogs, and their scroll areas do not
 * need individual Android WebView workarounds.
 */
export function EmbeddedViewportModal({
  usesEmbeddedViewport = false,
  ...props
}: EmbeddedViewportModalProps) {
  return <Modal {...props} styles={usesEmbeddedViewport ? embeddedViewportModalStyles : undefined} />;
}
