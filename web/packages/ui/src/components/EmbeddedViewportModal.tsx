import { Modal, type ModalProps } from "@mantine/core";
import type { CSSProperties } from "react";

type EmbeddedViewportModalProps = Omit<ModalProps, "styles"> & {
  /**
   * Android WebView can report a zero dynamic viewport height while a fixed
   * fullscreen surface is active. Mantine's default modal height uses dvh.
   */
  usesEmbeddedViewport?: boolean;
  /**
   * Fill the embedded WebView without passing Mantine's `fullScreen` prop,
   * which itself relies on a dynamic viewport height.
   */
  fillEmbeddedViewport?: boolean;
  contentStyle?: CSSProperties;
  headerStyle?: CSSProperties;
  bodyStyle?: CSSProperties;
};

/**
 * A viewport-safe Mantine modal for embedded WebViews. Keep viewport sizing in
 * this adapter so fullscreen content, dialogs, and their scroll areas do not
 * need individual Android WebView workarounds.
 */
export function EmbeddedViewportModal({
  usesEmbeddedViewport = false,
  fillEmbeddedViewport = false,
  contentStyle,
  headerStyle,
  bodyStyle,
  ...props
}: EmbeddedViewportModalProps) {
  const fillsViewport = usesEmbeddedViewport && fillEmbeddedViewport;

  return (
    <Modal
      {...props}
      styles={{
        root: usesEmbeddedViewport
          ? {
              "--modal-y-offset": fillsViewport ? "0px" : "16px",
              "--modal-x-offset": fillsViewport ? "0px" : "16px"
            }
          : undefined,
        // The modal inner container is fixed with top and bottom bounds, so
        // percentages resolve from the actual WebView bounds instead of dvh.
        content: {
          maxHeight: fillsViewport ? "100%" : usesEmbeddedViewport ? "calc(100% - 32px)" : undefined,
          height: fillsViewport ? "100%" : undefined,
          ...contentStyle
        },
        header: headerStyle,
        body: bodyStyle
      }}
    />
  );
}
