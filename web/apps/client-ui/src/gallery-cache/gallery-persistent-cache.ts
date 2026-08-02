const GALLERY_CACHE_DATABASE_NAME = "ironmesh-client-gallery-cache";
const GALLERY_CACHE_DATABASE_VERSION = 1;
const GALLERY_CACHE_RECORD_STORE = "records";
const GALLERY_CACHE_RECORD_SCHEMA_VERSION = 1;
const GALLERY_CACHE_MAX_AGE_MS = 30 * 24 * 60 * 60 * 1_000;
const GALLERY_CACHE_MAX_RECORDS = 256;

export type GalleryCacheRecordKind = "snapshots" | "entries";

type GalleryCacheRecord = {
  id: string;
  schemaVersion: number;
  scope: string;
  cacheKey: string;
  kind: GalleryCacheRecordKind;
  updatedAt: number;
  payload: unknown;
};

export type GalleryCacheHit<T> = {
  payload: T;
  updatedAt: number;
};

/**
 * A deliberately small IndexedDB adapter for successful gallery JSON payloads.
 * Records remain independently addressable so a future delta-feed consumer can
 * patch cached pages without replacing one opaque dehydrated cache blob.
 */
export class GalleryPersistentCache {
  private databasePromise: Promise<IDBDatabase | null> | null = null;
  private disabled = false;

  async read<T>(
    scope: string,
    cacheKey: string,
    validatePayload: (payload: unknown) => payload is T
  ): Promise<GalleryCacheHit<T> | null> {
    const database = await this.database();
    if (!database) {
      return null;
    }

    try {
      const transaction = database.transaction(GALLERY_CACHE_RECORD_STORE, "readwrite");
      const store = transaction.objectStore(GALLERY_CACHE_RECORD_STORE);
      const id = recordId(scope, cacheKey);
      const record = await requestResult<GalleryCacheRecord | undefined>(store.get(id));
      if (!isUsableRecord(record, scope, cacheKey, validatePayload)) {
        if (record !== undefined) {
          store.delete(id);
        }
        await transactionDone(transaction);
        return null;
      }

      if (Date.now() - record.updatedAt > GALLERY_CACHE_MAX_AGE_MS) {
        store.delete(id);
        await transactionDone(transaction);
        return null;
      }

      await transactionDone(transaction);
      return {
        payload: record.payload,
        updatedAt: record.updatedAt
      };
    } catch {
      this.disable();
      return null;
    }
  }

  async write<T>(
    scope: string,
    cacheKey: string,
    kind: GalleryCacheRecordKind,
    payload: T
  ): Promise<void> {
    const database = await this.database();
    if (!database) {
      return;
    }

    try {
      const transaction = database.transaction(GALLERY_CACHE_RECORD_STORE, "readwrite");
      const store = transaction.objectStore(GALLERY_CACHE_RECORD_STORE);
      const record: GalleryCacheRecord = {
        id: recordId(scope, cacheKey),
        schemaVersion: GALLERY_CACHE_RECORD_SCHEMA_VERSION,
        scope,
        cacheKey,
        kind,
        updatedAt: Date.now(),
        payload
      };
      store.put(record);
      const records = await requestResult<GalleryCacheRecord[]>(store.getAll());
      pruneRecords(store, records, record.id);
      await transactionDone(transaction);
    } catch {
      // Private browsing modes, quota failures and unsupported WebViews must
      // never make the live gallery unusable.
      this.disable();
    }
  }

  private async database(): Promise<IDBDatabase | null> {
    if (this.disabled || typeof indexedDB === "undefined") {
      return null;
    }
    if (!this.databasePromise) {
      this.databasePromise = openDatabase().catch(() => {
        this.disable();
        return null;
      });
    }
    return this.databasePromise;
  }

  private disable() {
    this.disabled = true;
    void this.databasePromise?.then((database) => database?.close());
    this.databasePromise = Promise.resolve(null);
  }
}

function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(GALLERY_CACHE_DATABASE_NAME, GALLERY_CACHE_DATABASE_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(GALLERY_CACHE_RECORD_STORE)) {
        database.createObjectStore(GALLERY_CACHE_RECORD_STORE, { keyPath: "id" });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("Failed to open gallery cache"));
    request.onblocked = () => reject(new Error("Gallery cache upgrade was blocked"));
  });
}

function recordId(scope: string, cacheKey: string): string {
  return `${scope}\u0000${cacheKey}`;
}

function isUsableRecord<T>(
  record: GalleryCacheRecord | undefined,
  scope: string,
  cacheKey: string,
  validatePayload: (payload: unknown) => payload is T
): record is GalleryCacheRecord & { payload: T } {
  return Boolean(
    record &&
      record.schemaVersion === GALLERY_CACHE_RECORD_SCHEMA_VERSION &&
      record.scope === scope &&
      record.cacheKey === cacheKey &&
      Number.isFinite(record.updatedAt) &&
      validatePayload(record.payload)
  );
}

function pruneRecords(store: IDBObjectStore, records: GalleryCacheRecord[], currentId: string) {
  const cutoff = Date.now() - GALLERY_CACHE_MAX_AGE_MS;
  const retained = records
    .filter((record) => {
      const valid =
        record.schemaVersion === GALLERY_CACHE_RECORD_SCHEMA_VERSION &&
        Number.isFinite(record.updatedAt) &&
        record.updatedAt >= cutoff;
      if (!valid && record.id !== currentId) {
        store.delete(record.id);
      }
      return valid;
    })
    .sort((left, right) => right.updatedAt - left.updatedAt);

  for (const record of retained.slice(GALLERY_CACHE_MAX_RECORDS)) {
    if (record.id !== currentId) {
      store.delete(record.id);
    }
  }
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("IndexedDB request failed"));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error ?? new Error("IndexedDB transaction failed"));
    transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
  });
}
