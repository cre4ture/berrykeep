export function normalizeStorePrefix(prefix: string): string {
  const trimmed = prefix.trim().replace(/^\/+/, "");
  if (!trimmed) {
    return "";
  }
  return `${trimmed.replace(/\/+$/, "")}/`;
}

export function normalizeStorePath(path: string, isPrefix: boolean): string {
  const trimmed = path.trim().replace(/^\/+/, "");
  if (!trimmed) {
    return "";
  }
  if (isPrefix || trimmed.endsWith("/")) {
    return `${trimmed.replace(/\/+$/, "")}/`;
  }
  return trimmed;
}

export function storeEntryName(path: string, isPrefix: boolean): string {
  const normalizedPath = normalizeStorePath(path, isPrefix).replace(/\/+$/, "");
  if (!normalizedPath) {
    return "";
  }
  return normalizedPath.split("/").pop() ?? normalizedPath;
}

export function parentStorePrefix(path: string): string {
  const normalized = path.replace(/\/+$/, "");
  if (!normalized.includes("/")) {
    return "";
  }
  return `${normalized.split("/").slice(0, -1).join("/")}/`;
}
