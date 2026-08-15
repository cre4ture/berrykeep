type MockStoreIndexEntry = {
  path: string;
  entry_type: "prefix" | "key";
};

type MockStoreIndexPrefixEntry = {
  path: string;
  entry_type: "prefix";
};

export function filterMockStoreEntriesToPrefix<T extends MockStoreIndexEntry>(
  entries: T[],
  requestedPrefix: string
): T[] {
  const normalizedPrefix = normalizeMockStorePath(requestedPrefix);
  if (!normalizedPrefix) {
    return entries;
  }

  const prefixWithSeparator = `${normalizedPrefix}/`;
  return entries.filter((entry) => {
    const normalizedPath = normalizeMockStorePath(entry.path);
    return normalizedPath !== normalizedPrefix && normalizedPath.startsWith(prefixWithSeparator);
  });
}

export function projectMockStoreTreeEntries<T extends MockStoreIndexEntry>(
  entries: T[],
  requestedPrefix: string,
  requestedDepth: number
): Array<T | MockStoreIndexPrefixEntry> {
  const normalizedPrefix = normalizeMockStorePath(requestedPrefix);
  const depth = Math.max(1, requestedDepth || 1);
  const projectedEntries = new Map<string, T | MockStoreIndexPrefixEntry>();

  for (const entry of entries) {
    const pathWithoutTrailingSlash = normalizeMockStorePath(entry.path);
    if (!pathWithoutTrailingSlash) {
      continue;
    }

    let relativePath = pathWithoutTrailingSlash;
    if (normalizedPrefix) {
      if (pathWithoutTrailingSlash === normalizedPrefix) {
        continue;
      }
      const prefixWithSeparator = `${normalizedPrefix}/`;
      if (!pathWithoutTrailingSlash.startsWith(prefixWithSeparator)) {
        continue;
      }
      relativePath = pathWithoutTrailingSlash.slice(prefixWithSeparator.length);
    }

    const relativeSegments = relativePath.split("/").filter(Boolean);
    if (relativeSegments.length === 0) {
      continue;
    }
    if (relativeSegments.length > depth) {
      const collapsedPath = [normalizedPrefix, ...relativeSegments.slice(0, depth)]
        .filter(Boolean)
        .join("/");
      const prefixPath = `${collapsedPath}/`;
      if (!projectedEntries.has(prefixPath)) {
        projectedEntries.set(prefixPath, { path: prefixPath, entry_type: "prefix" });
      }
      continue;
    }

    const normalizedPath =
      entry.entry_type === "prefix" ? `${pathWithoutTrailingSlash}/` : pathWithoutTrailingSlash;
    projectedEntries.set(normalizedPath, { ...entry, path: normalizedPath });
  }

  return [...projectedEntries.values()];
}

function normalizeMockStorePath(path: string): string {
  return path.trim().replace(/^\/+/, "").replace(/\/+$/, "");
}
