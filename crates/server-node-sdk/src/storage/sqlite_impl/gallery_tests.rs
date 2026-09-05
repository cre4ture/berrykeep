use super::*;
use crate::storage::{
    GalleryCaptureSummaryBusyError, GalleryIndexMediaFilter, GallerySummaryMiss,
    GalleryViewportBounds, MediaCacheStatus,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn sqlite_test_db_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ironmesh-{name}-{}-{stamp}.sqlite",
        std::process::id()
    ))
}

fn insert_gallery_fixture(
    db: &Connection,
    key: &str,
    media_type: &str,
    captured_at_unix: u64,
    latitude: Option<f64>,
    longitude: Option<f64>,
) {
    let spatial_position = latitude
        .zip(longitude)
        .and_then(|(latitude, longitude)| gallery_web_mercator_position(latitude, longitude));
    db.execute(
        "INSERT INTO gallery_objects (
             key, manifest_hash, object_id, inferred_media_type, media_type,
             captured_at_unix, media_status, geotagged, latitude, longitude,
             spatial_x, spatial_y
         ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, 'ready', ?6, ?7, ?8, ?9, ?10)",
        params![
            key,
            format!("manifest-{key}"),
            format!("object-{key}"),
            media_type,
            captured_at_unix,
            if latitude.is_some() { 1 } else { 0 },
            latitude,
            longitude,
            spatial_position.map(|position| position.0),
            spatial_position.map(|position| position.1),
        ],
    )
    .expect("gallery fixture should insert");
}

fn gallery_delta_scope() -> GalleryDeltaScope {
    GalleryDeltaScope {
        prefix: "gallery".to_string(),
        depth: 64,
        media_filter: GalleryIndexMediaFilter::All,
        captured_sort: GalleryIndexCapturedSort::Desc,
        viewport: None,
        label_filter: Default::default(),
    }
}

#[tokio::test]
async fn gallery_backfill_migrates_zero_capture_time_to_version_creation() {
    let metadata_db_path = sqlite_test_db_path("gallery-capture-fallback-backfill");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");
    let version_index_payload = serde_json::to_vec(&serde_json::json!({
        "object_id": "object-pending",
        "versions": {
            "version-pending": {
                "version_id": "version-pending",
                "object_id": "object-pending",
                "manifest_hash": "manifest-pending",
                "logical_path": "gallery/pending.jpg",
                "parent_version_ids": [],
                "state": "confirmed",
                "created_at_unix": 1_800_000_000u64,
                "copied_from_object_id": null,
                "copied_from_version_id": null,
                "copied_from_path": null
            }
        },
        "head_version_ids": ["version-pending"],
        "preferred_head_version_id": "version-pending"
    }))
    .unwrap();
    store
        .write_tx(move |db| {
            db.execute_batch(
                "INSERT INTO current_objects (key, manifest_hash, object_id)
                 VALUES ('gallery/pending.jpg', 'manifest-pending', 'object-pending');
                 INSERT INTO gallery_objects (
                     key, manifest_hash, object_id, inferred_media_type, media_type,
                     captured_at_unix, media_status, geotagged, latitude, longitude
                 ) VALUES (
                     'gallery/pending.jpg', 'manifest-pending', 'object-pending', 'image', 'image',
                     0, NULL, 0, NULL, NULL
                 );
                 DELETE FROM metadata_meta WHERE key = 'gallery_capture_fallback_v1';",
            )?;
            db.execute(
                "INSERT INTO version_indexes (object_id, index_json) VALUES (?1, ?2)",
                params!["object-pending", version_index_payload],
            )?;
            Ok(())
        })
        .await
        .expect("legacy gallery projection should persist");
    drop(store);

    let reopened = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should reopen");
    let captured_at_unix = reopened
        .read(|db| {
            db.query_row(
                "SELECT captured_at_unix FROM gallery_objects WHERE key = 'gallery/pending.jpg'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(Into::into)
        })
        .await
        .expect("backfilled gallery capture time should load");
    assert_eq!(captured_at_unix, 1_800_000_000);

    drop(reopened);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[test]
fn gallery_revision_and_delta_survive_restart_and_track_removals() {
    let metadata_db_path = sqlite_test_db_path("gallery-delta-restart");
    let history_id;
    {
        let db = Connection::open(&metadata_db_path).expect("sqlite should open");
        init_metadata_db(&db).expect("metadata schema should initialize");
        history_id = current_gallery_history_id_from_db(&db).unwrap();
        insert_gallery_fixture(&db, "gallery/cat.jpg", "image", 10, None, None);
        assert_eq!(current_gallery_revision_from_db(&db).unwrap(), 1);
    }

    let db = Connection::open(&metadata_db_path).expect("sqlite should reopen");
    init_metadata_db(&db).expect("metadata schema should reopen");
    assert_eq!(current_gallery_history_id_from_db(&db).unwrap(), history_id);
    assert_eq!(current_gallery_revision_from_db(&db).unwrap(), 1);
    db.execute(
        "UPDATE gallery_objects
         SET captured_at_unix = 20, geotagged = 1, latitude = 47.4, longitude = 8.5
         WHERE key = 'gallery/cat.jpg'",
        [],
    )
    .unwrap();
    let scope = gallery_delta_scope();
    let page = query_gallery_delta_from_db(&db, &history_id, 1, 10, &scope)
        .unwrap()
        .unwrap();
    assert_eq!(page.next_revision, 2);
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].kind, GalleryDeltaKind::Upsert);
    assert_eq!(page.changes[0].key, "gallery/cat.jpg");

    db.execute(
        "DELETE FROM gallery_objects WHERE key = 'gallery/cat.jpg'",
        [],
    )
    .unwrap();
    let removal = query_gallery_delta_from_db(&db, &history_id, 2, 10, &scope)
        .unwrap()
        .unwrap();
    assert_eq!(removal.next_revision, 3);
    assert_eq!(removal.changes[0].kind, GalleryDeltaKind::Removal);
    assert!(removal.changes[0].entry.is_none());

    let first_page = query_gallery_delta_from_db(&db, &history_id, 0, 1, &scope)
        .unwrap()
        .unwrap();
    assert!(first_page.has_more);
    assert_eq!(first_page.next_revision, 1);
    assert!(first_page.changes.is_empty());
    let second_page =
        query_gallery_delta_from_db(&db, &history_id, first_page.next_revision, 1, &scope)
            .unwrap()
            .unwrap();
    assert!(second_page.has_more);
    assert_eq!(second_page.next_revision, 2);
    assert_eq!(second_page.changes[0].kind, GalleryDeltaKind::Removal);
    let final_page =
        query_gallery_delta_from_db(&db, &history_id, second_page.next_revision, 1, &scope)
            .unwrap()
            .unwrap();
    assert!(!final_page.has_more);
    assert_eq!(final_page.next_revision, 3);
    assert_eq!(final_page.changes[0].kind, GalleryDeltaKind::Removal);

    let ahead = query_gallery_delta_from_db(&db, &history_id, 4, 10, &scope).unwrap();
    assert!(matches!(
        ahead,
        Err(GalleryDeltaCursorError::Ahead {
            current_revision: 3,
            ..
        })
    ));
    let foreign =
        query_gallery_delta_from_db(&db, &Uuid::new_v4().to_string(), 3, 10, &scope).unwrap();
    assert!(matches!(
        foreign,
        Err(GalleryDeltaCursorError::HistoryMismatch {
            current_revision: 3,
            ..
        })
    ));
    db.execute("DELETE FROM gallery_changes WHERE revision < 3", [])
        .unwrap();
    let expired = query_gallery_delta_from_db(&db, &history_id, 0, 10, &scope).unwrap();
    assert!(matches!(
        expired,
        Err(GalleryDeltaCursorError::Expired {
            current_revision: 3,
            ..
        })
    ));
    drop(db);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[test]
fn gallery_index_token_precedes_concurrent_writes_replayed_by_delta() {
    let metadata_db_path = sqlite_test_db_path("gallery-index-snapshot-token");
    let reader = Connection::open(&metadata_db_path).expect("reader should open");
    init_metadata_db(&reader).expect("metadata schema should initialize");
    insert_gallery_fixture(&reader, "gallery/first.jpg", "image", 10, None, None);

    let transaction = reader
        .unchecked_transaction()
        .expect("read transaction should begin");
    let history_id = current_gallery_history_id_from_db(&transaction).unwrap();
    let revision = current_gallery_revision_from_db(&transaction).unwrap();
    assert_eq!(revision, 1);

    let writer = Connection::open(&metadata_db_path).expect("writer should open");
    init_metadata_db(&writer).expect("metadata schema should initialize for writer");
    insert_gallery_fixture(&writer, "gallery/second.jpg", "image", 20, None, None);

    let page = query_gallery_index_in_transaction(
        &transaction,
        &GalleryIndexQuery {
            prefix: "gallery".to_string(),
            depth: 2,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_sort: GalleryIndexCapturedSort::Desc,
            captured_from_unix: None,
            captured_until_unix: None,
            offset: 0,
            limit: 10,
            viewport: None,
            label_filter: Default::default(),
        },
        history_id.clone(),
        revision,
    )
    .unwrap();
    assert_eq!(page.revision, 1);
    assert_eq!(page.entries.len(), 1);
    transaction.commit().unwrap();

    let delta = query_gallery_delta_from_db(
        &reader,
        &history_id,
        page.revision,
        10,
        &gallery_delta_scope(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(delta.next_revision, 2);
    assert_eq!(delta.changes.len(), 1);
    assert_eq!(delta.changes[0].key, "gallery/second.jpg");
    assert_eq!(delta.changes[0].kind, GalleryDeltaKind::Upsert);

    drop(writer);
    drop(reader);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[test]
fn gallery_viewport_query_filters_and_wraps_antimeridian() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(
        &db,
        "gallery/zurich.jpg",
        "image",
        30,
        Some(47.4),
        Some(8.5),
    );
    insert_gallery_fixture(
        &db,
        "gallery/fiji.jpg",
        "image",
        20,
        Some(-17.7),
        Some(178.1),
    );
    insert_gallery_fixture(
        &db,
        "gallery/samoa.jpg",
        "image",
        10,
        Some(-13.8),
        Some(-171.8),
    );
    insert_gallery_fixture(
        &db,
        "gallery/fiji.mp4",
        "video",
        40,
        Some(-17.8),
        Some(179.0),
    );
    insert_gallery_fixture(&db, "gallery/no-gps.jpg", "image", 50, None, None);

    let query = |viewport, media_filter| GalleryIndexQuery {
        prefix: "gallery".to_string(),
        depth: 2,
        media_filter,
        captured_sort: GalleryIndexCapturedSort::Desc,
        captured_from_unix: None,
        captured_until_unix: None,
        offset: 0,
        limit: 10,
        viewport: Some(viewport),
        label_filter: Default::default(),
    };
    let zurich = query_gallery_index_from_db(
        &db,
        &query(
            GalleryViewportBounds {
                south: 45.0,
                west: 5.0,
                north: 49.0,
                east: 11.0,
            },
            GalleryIndexMediaFilter::Image,
        ),
    )
    .unwrap();
    assert_eq!(
        zurich
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["gallery/zurich.jpg"]
    );

    let dateline = query_gallery_index_from_db(
        &db,
        &query(
            GalleryViewportBounds {
                south: -20.0,
                west: 170.0,
                north: -10.0,
                east: -170.0,
            },
            GalleryIndexMediaFilter::Image,
        ),
    )
    .unwrap();
    assert_eq!(
        dateline
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["gallery/fiji.jpg", "gallery/samoa.jpg"]
    );
    assert_eq!(dateline.total_entry_count, 2);
    assert_eq!(dateline.media_summary.geotagged_count, 2);
}

#[test]
fn gallery_index_filters_capture_time_with_inclusive_start_and_exclusive_end() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(&db, "gallery/older.jpg", "image", 10, None, None);
    insert_gallery_fixture(&db, "gallery/in-range.jpg", "image", 20, None, None);
    insert_gallery_fixture(&db, "gallery/upper-bound.jpg", "image", 30, None, None);

    let query = GalleryIndexQuery {
        prefix: "gallery".to_string(),
        depth: 2,
        media_filter: GalleryIndexMediaFilter::Image,
        captured_sort: GalleryIndexCapturedSort::Desc,
        captured_from_unix: Some(20),
        captured_until_unix: Some(30),
        offset: 0,
        limit: 10,
        viewport: None,
        label_filter: Default::default(),
    };
    let page = query_gallery_index_from_db(&db, &query)
        .expect("capture-time filtered gallery should load");

    assert_eq!(page.total_entry_count, 1);
    assert_eq!(page.media_summary.image_count, 1);
    assert_eq!(page.entries[0].key, "gallery/in-range.jpg");

    let empty_page = query_gallery_index_from_db(
        &db,
        &GalleryIndexQuery {
            captured_until_unix: Some(20),
            ..query
        },
    )
    .expect("empty capture-time interval should load");
    assert_eq!(empty_page.total_entry_count, 0);
    assert!(empty_page.entries.is_empty());
}

fn gallery_map_query(
    viewport: GalleryViewportBounds,
    requested_resolution: u32,
    max_clusters: usize,
) -> GalleryMapClusterQuery {
    GalleryMapClusterQuery {
        prefix: "gallery".to_string(),
        depth: 64,
        media_filter: GalleryIndexMediaFilter::Image,
        captured_from_unix: None,
        captured_until_unix: None,
        viewport,
        requested_resolution,
        max_clusters,
        label_filter: Default::default(),
    }
}

#[test]
fn gallery_map_clusters_are_bounded_and_preserve_scope_summary() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(
        &db,
        "gallery/zurich.jpg",
        "image",
        30,
        Some(47.4),
        Some(8.5),
    );
    insert_gallery_fixture(
        &db,
        "gallery/new-york.jpg",
        "image",
        20,
        Some(40.7),
        Some(-74.0),
    );
    insert_gallery_fixture(
        &db,
        "gallery/sydney.jpg",
        "image",
        10,
        Some(-33.9),
        Some(151.2),
    );
    insert_gallery_fixture(&db, "gallery/no-gps.jpg", "image", 40, None, None);
    insert_gallery_fixture(&db, "gallery/video.mp4", "video", 50, Some(47.4), Some(8.5));

    let page = query_gallery_map_clusters_from_db(
        &db,
        &gallery_map_query(
            GalleryViewportBounds {
                south: -90.0,
                west: -180.0,
                north: 90.0,
                east: 180.0,
            },
            4096,
            1,
        ),
    )
    .expect("gallery clusters should load");

    assert_eq!(page.total_entry_count, 4);
    assert_eq!(page.media_summary.image_count, 4);
    assert_eq!(page.media_summary.video_count, 0);
    assert_eq!(page.media_summary.geotagged_count, 3);
    assert_eq!(page.visible_geotagged_count, 3);
    assert_eq!(page.clusters.len(), 1);
    assert!(page.resolution < 4096);
    assert_eq!(page.clusters[0].count, 3);
    assert!(page.clusters[0].entry.is_none());
}

#[test]
fn gallery_map_filters_capture_time_in_clusters_summary_and_entries() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(&db, "gallery/older.jpg", "image", 10, Some(47.4), Some(8.5));
    insert_gallery_fixture(
        &db,
        "gallery/in-range.jpg",
        "image",
        20,
        Some(47.4),
        Some(8.5),
    );
    insert_gallery_fixture(
        &db,
        "gallery/upper-bound.jpg",
        "image",
        30,
        Some(47.4),
        Some(8.5),
    );
    let viewport = GalleryViewportBounds {
        south: -90.0,
        west: -180.0,
        north: 90.0,
        east: 180.0,
    };
    let mut query = gallery_map_query(viewport, 1, 10);
    query.captured_from_unix = Some(20);
    query.captured_until_unix = Some(30);

    let clusters = query_gallery_map_clusters_from_db(&db, &query)
        .expect("capture-time filtered gallery map should load");
    assert_eq!(clusters.total_entry_count, 1);
    assert_eq!(clusters.media_summary.image_count, 1);
    assert_eq!(clusters.visible_geotagged_count, 1);
    assert_eq!(clusters.clusters.len(), 1);
    assert_eq!(
        clusters.clusters[0]
            .entry
            .as_ref()
            .map(|entry| entry.key.as_str()),
        Some("gallery/in-range.jpg")
    );

    let cluster = &clusters.clusters[0];
    let entries = query_gallery_map_cluster_entries_from_db(
        &db,
        &GalleryMapClusterEntriesQuery {
            prefix: query.prefix.clone(),
            depth: query.depth,
            media_filter: query.media_filter,
            captured_from_unix: query.captured_from_unix,
            captured_until_unix: query.captured_until_unix,
            viewport,
            resolution: clusters.resolution,
            cell_x: cluster.cell_x,
            cell_y: cluster.cell_y,
            offset: 0,
            limit: 10,
            label_filter: Default::default(),
        },
    )
    .expect("capture-time filtered cluster entries should load");
    assert_eq!(entries.total_entry_count, 1);
    assert_eq!(entries.entries[0].key, "gallery/in-range.jpg");

    query.captured_from_unix = Some(0);
    query.captured_until_unix = Some(0);
    let empty_clusters = query_gallery_map_clusters_from_db(&db, &query)
        .expect("empty capture-time interval should load");
    assert_eq!(empty_clusters.total_entry_count, 0);
    assert_eq!(empty_clusters.visible_geotagged_count, 0);
    assert!(empty_clusters.clusters.is_empty());
}

#[test]
fn gallery_map_capture_summary_uses_the_capture_time_index() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    let scope_values = vec![
        Value::Text(String::new()),
        Value::Text("%".to_string()),
        Value::Integer(64),
        Value::Null,
        Value::Integer(20),
        Value::Integer(30),
    ];
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM gallery_objects
         WHERE {GALLERY_MAP_SCOPE_SQL}{GALLERY_MAP_CAPTURE_RANGE_SQL}"
    );
    let mut statement = db.prepare(&sql).expect("query plan should prepare");
    let plan = statement
        .query_map(params_from_iter(scope_values), |row| {
            row.get::<_, String>(3)
        })
        .expect("query plan should execute")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("query plan rows should decode");

    assert!(
        plan.iter()
            .any(|step| step.contains("idx_gallery_objects_capture_summary")),
        "capture-filtered summary should use capture index: {plan:?}"
    );
}

#[test]
fn unfiltered_gallery_map_viewport_does_not_use_the_capture_summary_index() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    let viewport = GalleryViewportBounds {
        south: 40.0,
        west: 0.0,
        north: 60.0,
        east: 20.0,
    };
    let scope_values = sqlite_gallery_map_scope_values(
        "",
        "%",
        64,
        GalleryIndexMediaFilter::All,
        None,
        None,
        viewport,
    )
    .expect("unfiltered gallery map values should build");
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM gallery_objects
         WHERE {GALLERY_MAP_SCOPE_SQL}{GALLERY_MAP_OPTIONAL_CAPTURE_SQL}
           AND {GALLERY_MAP_VIEWPORT_SQL}"
    );
    let mut statement = db.prepare(&sql).expect("query plan should prepare");
    let plan = statement
        .query_map(params_from_iter(scope_values), |row| {
            row.get::<_, String>(3)
        })
        .expect("query plan should execute")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("query plan rows should decode");

    assert!(
        plan.iter()
            .all(|step| !step.contains("idx_gallery_objects_capture_summary")),
        "unfiltered viewport must not use the capture-summary index: {plan:?}"
    );
    assert!(
        plan.iter().any(|step| {
            step.contains("idx_gallery_objects_spatial")
                || step.contains("idx_gallery_objects_viewport")
        }),
        "unfiltered viewport should retain a viewport-bounded plan: {plan:?}"
    );
}

#[tokio::test]
async fn gallery_map_summary_cache_serves_stale_value_and_refreshes_in_background() {
    let metadata_db_path = sqlite_test_db_path("gallery-map-summary-cache");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");
    store
        .write_tx(|db| {
            insert_gallery_fixture(db, "gallery/a.jpg", "image", 1, Some(47.4), Some(8.5));
            Ok(())
        })
        .await
        .unwrap();

    let viewport = GalleryViewportBounds {
        south: -90.0,
        west: -180.0,
        north: 90.0,
        east: 180.0,
    };
    let query = gallery_map_query(viewport, 1024, 512);

    // Cold cache: computed synchronously, so the very first caller still gets a real answer.
    let first = store
        .query_gallery_map_clusters(&query)
        .await
        .unwrap()
        .expect("gallery map clusters should be available");
    assert_eq!(first.total_entry_count, 1);
    assert!(!first.summary_status.refreshing);

    store
        .write_tx(|db| {
            insert_gallery_fixture(db, "gallery/b.jpg", "image", 2, Some(47.4), Some(8.5));
            Ok(())
        })
        .await
        .unwrap();

    // Warm but stale cache: the (now outdated) cached summary is served immediately rather than
    // blocking this request on a recompute, while a background refresh is kicked off.
    let second = store
        .query_gallery_map_clusters(&query)
        .await
        .unwrap()
        .expect("gallery map clusters should be available");
    assert_eq!(second.total_entry_count, 1);
    assert!(second.summary_status.refreshing);
    // The viewport-bounded clusters themselves are never served from the summary cache.
    assert_eq!(second.visible_geotagged_count, 2);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let refreshed = store
            .query_gallery_map_clusters(&query)
            .await
            .unwrap()
            .expect("gallery map clusters should be available");
        if refreshed.total_entry_count == 2 && !refreshed.summary_status.refreshing {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background gallery map summary refresh did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    drop(store);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[tokio::test]
async fn gallery_map_rejects_excess_capture_summary_misses_before_reading_the_viewport() {
    let metadata_db_path = sqlite_test_db_path("gallery-map-summary-early-shed");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");

    let capture_scope = |captured_from_unix| GallerySummaryScope {
        prefix: "gallery".to_string(),
        depth: 64,
        media_filter: GalleryIndexMediaFilter::All,
        captured_from_unix: Some(captured_from_unix),
        captured_until_unix: None,
        label_filter: Default::default(),
    };
    let _first_permit = match store
        .gallery_map_summary_cache
        .try_start_summary_miss(&capture_scope(1))
        .unwrap()
    {
        GallerySummaryMiss::Leader(permit) => permit,
        GallerySummaryMiss::Follower(_) => panic!("first capture miss should be admitted"),
    };
    let _second_permit = match store
        .gallery_map_summary_cache
        .try_start_summary_miss(&capture_scope(2))
        .unwrap()
    {
        GallerySummaryMiss::Leader(permit) => permit,
        GallerySummaryMiss::Follower(_) => panic!("second capture miss should be admitted"),
    };

    let viewport = GalleryViewportBounds {
        south: -90.0,
        west: -180.0,
        north: 90.0,
        east: 180.0,
    };
    let mut query = gallery_map_query(viewport, 1024, 512);
    query.captured_from_unix = Some(3);
    let next_reader_before = store.next_reader.load(std::sync::atomic::Ordering::Relaxed);
    let error = store
        .query_gallery_map_clusters(&query)
        .await
        .expect_err("a third concurrent capture miss should be rejected");

    assert!(
        error
            .downcast_ref::<GalleryCaptureSummaryBusyError>()
            .is_some()
    );
    assert_eq!(
        store.next_reader.load(std::sync::atomic::Ordering::Relaxed),
        next_reader_before,
        "the viewport query must not run after capture-summary admission is rejected"
    );

    drop(store);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[test]
fn gallery_map_cluster_entries_are_paginated_in_capture_order() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(&db, "gallery/older.jpg", "image", 10, Some(47.4), Some(8.5));
    insert_gallery_fixture(&db, "gallery/newer.jpg", "image", 20, Some(47.4), Some(8.5));

    let viewport = GalleryViewportBounds {
        south: 45.0,
        west: 5.0,
        north: 49.0,
        east: 11.0,
    };
    let clusters = query_gallery_map_clusters_from_db(&db, &gallery_map_query(viewport, 1024, 512))
        .expect("gallery clusters should load");
    assert_eq!(clusters.clusters.len(), 1);
    let cluster = &clusters.clusters[0];
    assert_eq!(cluster.count, 2);
    assert!(cluster.entry.is_none());

    let first_page = query_gallery_map_cluster_entries_from_db(
        &db,
        &GalleryMapClusterEntriesQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_from_unix: None,
            captured_until_unix: None,
            viewport,
            resolution: clusters.resolution,
            cell_x: cluster.cell_x,
            cell_y: cluster.cell_y,
            offset: 0,
            limit: 1,
            label_filter: Default::default(),
        },
    )
    .expect("first cluster entry page should load");
    assert_eq!(first_page.total_entry_count, 2);
    assert_eq!(first_page.entries.len(), 1);
    assert_eq!(first_page.entries[0].key, "gallery/newer.jpg");

    let second_page = query_gallery_map_cluster_entries_from_db(
        &db,
        &GalleryMapClusterEntriesQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_from_unix: None,
            captured_until_unix: None,
            viewport,
            resolution: clusters.resolution,
            cell_x: cluster.cell_x,
            cell_y: cluster.cell_y,
            offset: 1,
            limit: 1,
            label_filter: Default::default(),
        },
    )
    .expect("second cluster entry page should load");
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(second_page.entries[0].key, "gallery/older.jpg");
    assert_eq!(first_page.history_id, clusters.history_id);
    assert_eq!(first_page.revision, clusters.revision);
}

#[test]
fn gallery_spatial_backfill_restores_missing_positions() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    db.execute(
        "INSERT INTO gallery_objects(
             key, manifest_hash, object_id, inferred_media_type, media_type,
             captured_at_unix, media_status, geotagged, latitude, longitude
         ) VALUES (?1, ?2, ?3, 'image', 'image', 1, 'ready', 1, 47.4, 8.5)",
        params!["gallery/missing-position.jpg", "manifest", "object"],
    )
    .expect("gallery fixture should insert");

    backfill_gallery_spatial_positions(&db).expect("spatial positions should backfill");
    let (spatial_x, spatial_y) = db
        .query_row(
            "SELECT spatial_x, spatial_y FROM gallery_objects WHERE key = ?1",
            ["gallery/missing-position.jpg"],
            |row| Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, Option<f64>>(1)?)),
        )
        .expect("backfilled gallery fixture should load");
    assert!(spatial_x.is_some());
    assert!(spatial_y.is_some());
}

#[tokio::test]
async fn gallery_map_cluster_tokens_ignore_changes_outside_their_scope() {
    let metadata_db_path = sqlite_test_db_path("gallery-map-token-scope");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");
    store
        .write_tx(|db| {
            insert_gallery_fixture(db, "gallery/a.jpg", "image", 10, Some(47.4), Some(8.5));
            insert_gallery_fixture(db, "gallery/b.jpg", "image", 20, Some(47.4), Some(8.5));
            Ok(())
        })
        .await
        .expect("gallery fixtures should persist");

    let viewport = GalleryViewportBounds {
        south: 45.0,
        west: 5.0,
        north: 49.0,
        east: 11.0,
    };
    let clusters = store
        .query_gallery_map_clusters(&gallery_map_query(viewport, 1024, 512))
        .await
        .expect("gallery map clusters should load")
        .expect("sqlite should support gallery map clusters");
    let cluster = clusters
        .clusters
        .first()
        .expect("fixture should produce one map cluster");

    store
        .write_tx(|db| {
            insert_gallery_fixture(
                db,
                "elsewhere/noise.jpg",
                "image",
                30,
                Some(40.7),
                Some(-74.0),
            );
            Ok(())
        })
        .await
        .expect("out-of-scope fixture should persist");

    let page = store
        .query_gallery_map_cluster_entries(&GalleryMapClusterEntriesQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_from_unix: None,
            captured_until_unix: None,
            viewport,
            resolution: clusters.resolution,
            cell_x: cluster.cell_x,
            cell_y: cluster.cell_y,
            offset: 0,
            limit: 100,
            label_filter: Default::default(),
        })
        .await
        .expect("cluster page should load after unrelated ingest")
        .expect("sqlite should support cluster pages");

    assert_eq!(page.history_id, clusters.history_id);
    assert_eq!(page.revision, clusters.revision);

    store
        .write_tx(|db| {
            db.execute(
                "UPDATE gallery_objects SET captured_at_unix = 40 WHERE key = 'gallery/a.jpg'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("in-scope fixture should update");
    let changed_page = store
        .query_gallery_map_cluster_entries(&GalleryMapClusterEntriesQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_from_unix: None,
            captured_until_unix: None,
            viewport,
            resolution: clusters.resolution,
            cell_x: cluster.cell_x,
            cell_y: cluster.cell_y,
            offset: 0,
            limit: 100,
            label_filter: Default::default(),
        })
        .await
        .expect("cluster page should load after in-scope change")
        .expect("sqlite should support cluster pages");
    assert_ne!(changed_page.revision, clusters.revision);

    drop(store);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[test]
fn gallery_map_clusters_filter_an_antimeridian_viewport() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(
        &db,
        "gallery/fiji.jpg",
        "image",
        30,
        Some(-17.7),
        Some(178.1),
    );
    insert_gallery_fixture(
        &db,
        "gallery/samoa.jpg",
        "image",
        20,
        Some(-13.8),
        Some(-171.8),
    );
    insert_gallery_fixture(
        &db,
        "gallery/zurich.jpg",
        "image",
        10,
        Some(47.4),
        Some(8.5),
    );

    let page = query_gallery_map_clusters_from_db(
        &db,
        &gallery_map_query(
            GalleryViewportBounds {
                south: -20.0,
                west: 170.0,
                north: -10.0,
                east: -170.0,
            },
            1024,
            512,
        ),
    )
    .expect("antimeridian clusters should load");

    assert_eq!(page.total_entry_count, 3);
    assert_eq!(page.media_summary.geotagged_count, 3);
    assert_eq!(page.visible_geotagged_count, 2);
    assert_eq!(
        page.clusters
            .iter()
            .map(|cluster| cluster.count)
            .sum::<usize>(),
        2
    );
}

#[test]
fn gallery_delta_reconciles_entries_entering_and_leaving_token_scope() {
    let db = Connection::open_in_memory().expect("in-memory sqlite should open");
    init_metadata_db(&db).expect("metadata schema should initialize");
    insert_gallery_fixture(
        &db,
        "gallery/in-scope.jpg",
        "image",
        30,
        Some(47.4),
        Some(8.5),
    );
    insert_gallery_fixture(
        &db,
        "gallery/outside.jpg",
        "image",
        20,
        Some(47.4),
        Some(20.0),
    );
    insert_gallery_fixture(&db, "gallery/video.mp4", "video", 10, Some(47.4), Some(8.5));
    let history_id = current_gallery_history_id_from_db(&db).unwrap();
    let revision = current_gallery_revision_from_db(&db).unwrap();
    let scope = GalleryDeltaScope {
        prefix: "gallery".to_string(),
        depth: 2,
        media_filter: GalleryIndexMediaFilter::Image,
        captured_sort: GalleryIndexCapturedSort::Desc,
        viewport: Some(GalleryViewportBounds {
            south: 45.0,
            west: 5.0,
            north: 49.0,
            east: 11.0,
        }),
        label_filter: Default::default(),
    };

    db.execute(
        "UPDATE gallery_objects SET longitude = 20.0 WHERE key = 'gallery/in-scope.jpg'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE gallery_objects SET longitude = 8.5 WHERE key = 'gallery/outside.jpg'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE gallery_objects SET media_type = 'image' WHERE key = 'gallery/video.mp4'",
        [],
    )
    .unwrap();

    let page = query_gallery_delta_from_db(&db, &history_id, revision, 10, &scope)
        .unwrap()
        .unwrap();
    assert_eq!(page.changes.len(), 3);
    assert_eq!(page.changes[0].key, "gallery/in-scope.jpg");
    assert_eq!(page.changes[0].kind, GalleryDeltaKind::Removal);
    assert_eq!(page.changes[1].key, "gallery/outside.jpg");
    assert_eq!(page.changes[1].kind, GalleryDeltaKind::Upsert);
    assert_eq!(page.changes[2].key, "gallery/video.mp4");
    assert_eq!(page.changes[2].kind, GalleryDeltaKind::Upsert);

    db.execute(
        "UPDATE gallery_objects SET media_type = 'video' WHERE key = 'gallery/outside.jpg'",
        [],
    )
    .unwrap();
    let next = query_gallery_delta_from_db(&db, &history_id, page.next_revision, 10, &scope)
        .unwrap()
        .unwrap();
    assert_eq!(next.changes.len(), 1);
    assert_eq!(next.changes[0].key, "gallery/outside.jpg");
    assert_eq!(next.changes[0].kind, GalleryDeltaKind::Removal);

    let outside_revision = next.next_revision;
    insert_gallery_fixture(&db, "other/outside.jpg", "image", 1, Some(47.4), Some(8.5));
    db.execute(
        "UPDATE gallery_objects SET captured_at_unix = 2 WHERE key = 'other/outside.jpg'",
        [],
    )
    .unwrap();
    let omitted_first = query_gallery_delta_from_db(&db, &history_id, outside_revision, 1, &scope)
        .unwrap()
        .unwrap();
    assert!(omitted_first.has_more);
    assert!(omitted_first.changes.is_empty());
    assert_eq!(omitted_first.next_revision, outside_revision + 1);
    let omitted_final =
        query_gallery_delta_from_db(&db, &history_id, omitted_first.next_revision, 1, &scope)
            .unwrap()
            .unwrap();
    assert!(!omitted_final.has_more);
    assert!(omitted_final.changes.is_empty());
    assert_eq!(omitted_final.next_revision, outside_revision + 2);
}

#[tokio::test]
async fn gallery_media_delta_tracks_unprojected_changes_without_noop_churn() {
    let metadata_db_path = sqlite_test_db_path("gallery-media-delta-noop");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");
    store
        .write_tx(|db| {
            db.execute(
                "INSERT INTO manifest_summaries
                     (manifest_hash, total_size_bytes, content_fingerprint)
                 VALUES ('manifest-cat', 100, 'fingerprint-cat')",
                [],
            )?;
            insert_gallery_fixture(db, "gallery/cat.jpg", "image", 10, None, None);
            db.execute(
                "UPDATE gallery_objects SET manifest_hash = 'manifest-cat'
                 WHERE key = 'gallery/cat.jpg'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut metadata = CachedMediaMetadata {
        schema_version: crate::storage::media_cache::MEDIA_CACHE_SCHEMA_VERSION,
        content_fingerprint: "fingerprint-cat".to_string(),
        source_manifest_hash: "manifest-cat".to_string(),
        status: MediaCacheStatus::Ready,
        media_type: Some("image".to_string()),
        mime_type: Some("image/jpeg".to_string()),
        width: Some(64),
        height: Some(48),
        orientation: Some(1),
        taken_at_unix: Some(10),
        date_encoded_unix: None,
        duration_millis: None,
        frame_rate_millihertz: None,
        total_bitrate_bps: None,
        codec_name: None,
        codec_fourcc: None,
        gps: None,
        photo: None,
        thumbnail: None,
        source_size_bytes: 100,
        generated_at_unix: 2,
        retry_after_unix: None,
        error: None,
    };
    store.persist_media_cache_record(&metadata).await.unwrap();
    let (history_id, revision) = store
        .read(|db| {
            Ok((
                current_gallery_history_id_from_db(db)?,
                current_gallery_revision_from_db(db)?,
            ))
        })
        .await
        .unwrap();
    let scope = gallery_delta_scope();

    store.persist_media_cache_record(&metadata).await.unwrap();
    let noop = store
        .query_gallery_delta(&history_id, revision, 10, &scope)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(noop.changes.is_empty());
    assert_eq!(noop.next_revision, revision);

    metadata.width = Some(128);
    store.persist_media_cache_record(&metadata).await.unwrap();
    let changed = store
        .query_gallery_delta(&history_id, revision, 10, &scope)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(changed.changes.len(), 1);
    assert_eq!(changed.changes[0].kind, GalleryDeltaKind::Upsert);
    assert_eq!(
        changed.changes[0]
            .entry
            .as_ref()
            .and_then(|entry| entry.media_metadata.as_ref())
            .and_then(|metadata| metadata.width),
        Some(128)
    );

    drop(store);
    let _ = std::fs::remove_file(metadata_db_path);
}

#[tokio::test]
async fn gallery_media_delta_tracks_each_shared_content_fingerprint_entry() {
    let metadata_db_path = sqlite_test_db_path("gallery-media-delta-shared-fingerprint");
    let store = SqliteMetadataStore::open(&metadata_db_path)
        .await
        .expect("sqlite metadata store should open");
    store
        .write_tx(|db| {
            db.execute_batch(
                "
                INSERT INTO manifest_summaries (manifest_hash, total_size_bytes, content_fingerprint)
                VALUES
                    ('manifest-a', 100, 'fingerprint-shared'),
                    ('manifest-b', 100, 'fingerprint-shared');
                INSERT INTO gallery_objects (
                    key, manifest_hash, object_id, inferred_media_type, media_type,
                    captured_at_unix, media_status, geotagged, latitude, longitude
                ) VALUES
                    ('gallery/a.jpg', 'manifest-a', 'object-a', 'image', 'image', 10, 'ready', 0, NULL, NULL),
                    ('gallery/b.jpg', 'manifest-b', 'object-b', 'image', 'image', 10, 'ready', 0, NULL, NULL);
                ",
            )?;
            Ok(())
        })
        .await
        .expect("gallery fixtures should persist");

    let media_metadata = |width| CachedMediaMetadata {
        schema_version: crate::storage::media_cache::MEDIA_CACHE_SCHEMA_VERSION,
        content_fingerprint: "fingerprint-shared".to_string(),
        source_manifest_hash: "manifest-a".to_string(),
        status: MediaCacheStatus::Ready,
        media_type: Some("image".to_string()),
        mime_type: Some("image/jpeg".to_string()),
        width: Some(width),
        height: Some(48),
        orientation: Some(1),
        taken_at_unix: Some(10),
        date_encoded_unix: None,
        duration_millis: None,
        frame_rate_millihertz: None,
        total_bitrate_bps: None,
        codec_name: None,
        codec_fourcc: None,
        gps: None,
        photo: None,
        thumbnail: None,
        source_size_bytes: 100,
        generated_at_unix: 2,
        retry_after_unix: None,
        error: None,
    };
    store
        .persist_media_cache_record(&media_metadata(64))
        .await
        .expect("initial media metadata should persist");
    store
        .write_tx(|db| {
            db.execute(
                "UPDATE gallery_objects SET captured_at_unix = 0 WHERE key = 'gallery/a.jpg'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("first gallery entry should become stale");
    let (history_id, revision) = store
        .read(|db| {
            Ok((
                current_gallery_history_id_from_db(db)?,
                current_gallery_revision_from_db(db)?,
            ))
        })
        .await
        .expect("gallery cursor should load");

    store
        .persist_media_cache_record(&media_metadata(128))
        .await
        .expect("changed media metadata should persist");
    let page = store
        .query_gallery_delta(&history_id, revision, 10, &gallery_delta_scope())
        .await
        .expect("gallery delta should load")
        .expect("gallery backend should respond")
        .expect("gallery cursor should remain current");
    assert_eq!(page.changes.len(), 2);
    let mut changed_keys = page
        .changes
        .iter()
        .map(|change| change.key.as_str())
        .collect::<Vec<_>>();
    changed_keys.sort_unstable();
    assert_eq!(changed_keys, ["gallery/a.jpg", "gallery/b.jpg"]);
    assert!(page.changes.iter().all(|change| {
        change.kind == GalleryDeltaKind::Upsert
            && change
                .entry
                .as_ref()
                .and_then(|entry| entry.media_metadata.as_ref())
                .and_then(|metadata| metadata.width)
                == Some(128)
    }));

    drop(store);
    let _ = std::fs::remove_file(metadata_db_path);
}
