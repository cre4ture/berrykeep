export type StoreIndexGps = {
  latitude: number;
  longitude: number;
};

export type StoreIndexThumbnail = {
  url: string;
  profile: string;
  width: number;
  height: number;
  format: string;
  size_bytes: number;
};

export type StoreIndexMedia = {
  status: string;
  content_fingerprint: string;
  media_type?: string | null;
  mime_type?: string | null;
  width?: number | null;
  height?: number | null;
  orientation?: number | null;
  taken_at_unix?: number | null;
  gps?: StoreIndexGps | null;
  thumbnail?: StoreIndexThumbnail | null;
  error?: string | null;
};

export type StoreIndexMediaSummary = {
  ready_count: number;
  pending_count: number;
  incomplete_count: number;
  image_count: number;
  video_count: number;
  geotagged_count: number;
};

export type StoreIndexEntry = {
  path: string;
  entry_type: string;
  version?: string | null;
  content_hash?: string | null;
  size_bytes?: number | null;
  modified_at_unix?: number | null;
  content_fingerprint?: string | null;
  media?: StoreIndexMedia | null;
};

export type StoreIndexResponse = {
  prefix: string;
  depth: number;
  entry_count: number;
  total_entry_count: number;
  offset: number;
  limit?: number | null;
  has_more: boolean;
  next_cursor?: string | null;
  sync_token?: string | null;
  consistency_token?: string | null;
  media_summary: StoreIndexMediaSummary;
  entries: StoreIndexEntry[];
};

export type StoreIndexDeltaResponse = {
  next_token: string;
  has_more: boolean;
  upserts: StoreIndexEntry[];
  removals: string[];
};

export type StoreIndexDeltaResetError = {
  code: "store_index_delta_reset_required" | "store_index_delta_invalid_token";
  reset: true;
  message: string;
  current_token?: string | null;
};

export type StoreIndexViewport = {
  south: number;
  west: number;
  north: number;
  east: number;
};

export type StoreListView = "raw" | "tree";

export type StoreListSortOrder =
  | "captured_asc"
  | "captured_desc"
  | "modified_asc"
  | "modified_desc"
  | "path_asc"
  | "path_desc"
  | "size_asc"
  | "size_desc"
  | "type_asc"
  | "type_desc";

export type StoreListMediaFilter = "all" | "image" | "video";

export type StoreListRequestOptions = {
  view?: StoreListView;
  offset?: number;
  limit?: number;
  sort?: StoreListSortOrder;
  mediaFilter?: StoreListMediaFilter;
  viewport?: StoreIndexViewport;
};
