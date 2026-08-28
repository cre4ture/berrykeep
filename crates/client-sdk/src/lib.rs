pub mod bootstrap;
pub mod client_node;
pub mod connection;
pub mod content_addressed_client_cache;
pub mod device_auth;
mod iroh_lease_budget;
pub mod ironmesh_client;
pub mod latency_probe;
mod managed_client;
pub mod remote_sync;
mod route_supervisor;
mod session_pool;
mod staged_download_coordination;

pub use bootstrap::{
    BootstrapEnrollmentResult, ConnectionBootstrap, ConnectionBootstrapDiagnosticTargets,
    EnrolledClientConnection, PersistedRendezvousContactList, PlannedConnectionBootstrapTarget,
    ResolvedConnectionBootstrap, enroll_bootstrap_claim_blocking,
    enroll_client_connection_blocking, enroll_connection_input_blocking,
};
pub use client_node::ClientNode;
pub use connection::{
    build_blocking_http_client, build_blocking_reqwest_client_from_pem,
    build_blocking_reqwest_client_from_pem_for_url,
    build_client_with_optional_identity_from_planned_target,
    build_client_with_optional_identity_from_planned_targets, build_http_client,
    build_http_client_from_pem, build_http_client_from_planned_targets,
    build_http_client_with_identity, build_http_client_with_identity_from_pem,
    build_http_client_with_identity_from_planned_target,
    build_http_client_with_identity_from_planned_targets, build_reqwest_client_from_pem,
    build_reqwest_client_from_pem_for_url, load_root_certificate, load_root_certificate_pem,
};
pub use content_addressed_client_cache::ContentAddressedClientCache;
pub use device_auth::{
    DeviceEnrollmentRequest, DeviceEnrollmentResponse, RenewRendezvousIdentityResponse,
    enroll_device, enroll_device_blocking, enroll_device_blocking_from_pem,
    renew_rendezvous_identity,
};
pub use ironmesh_client::{
    ClientConnectionAttempt, ClientConnectionDiagnosticImpact, ClientConnectionDiagnostics,
    ClientConnectionDiagnosticsEvent, ClientConnectionOperationResult,
    ClientConnectionRouteEndpointSnapshot, ClientConnectionRouteSnapshot,
    ClientEndpointDiagnostics, ClientRouteMaintenancePolicy, ClientSnapshotInfo, GalleryMapBounds,
    GalleryMapCluster, GalleryMapClusterEntriesResponse, GalleryMapClustersRequest,
    GalleryMapClustersResponse, GallerySummaryStatus, IronMeshClient, ObjectHeadInfo,
    PreferredHeadReason, RequestedRange, SnapshotRestoreResponse, StoreIndexDeltaResponse,
    StoreIndexEntry, StoreIndexMediaFilter, StoreIndexMediaSummary, StoreIndexRequestOptions,
    StoreIndexResponse, StoreIndexSortOrder, StoreIndexView, StoreIndexViewport, UploadMode,
    UploadResult, UploadSessionChunkRef, UploadSessionChunkStatus, UploadSessionCompleteInfo,
    UploadSessionStatus, VersionConsistencyState, VersionGraphSummary, VersionRecordSummary,
    WebServiceProxyConnection, WebServiceSummary, normalize_server_base_url,
    set_connection_diagnostics_observer, snapshot_from_store_index_entries,
};
pub use latency_probe::{
    LatencyProbeAssessment, LatencyProbeComparison, LatencyProbeConfig, LatencyProbeResult,
    LatencyProbeSample, LatencyProbeSummary, TITLE_LATENCY_PROBE_DEFAULT_PERIOD_SECONDS,
    TITLE_LATENCY_PROBE_MAX_PERIOD_SECONDS, TITLE_LATENCY_PROBE_MIN_PERIOD_SECONDS,
    TitleLatencyConnectionType, TitleLatencyMonitor, TitleLatencyProbeConfig,
    TitleLatencyProbeState, TitleLatencyProbeStatus, compare_direct_and_relay_latency,
};
pub use managed_client::{
    ManagedBootstrapPersistence, ManagedClientOptions, ManagedIronMeshClient, RouteDiscoveryError,
    RouteRefreshOutcome, RouteRefreshReason,
};
pub use remote_sync::{
    RemoteSnapshotFetchProgress, RemoteSnapshotFetcher, RemoteSnapshotPoller, RemoteSnapshotScope,
    RemoteSnapshotUpdate, RemoteSyncScheduler, RemoteSyncStrategy, changed_paths_between,
};
pub use session_pool::TransportSessionPoolSnapshot;
pub use transport_sdk::{
    BootstrapEndpoint, BootstrapEndpointUse, BootstrapTrustRoots, ClientBootstrapClaim,
    ClientBootstrapClaimIssueResponse, ClientBootstrapClaimRedeemRequest,
    ClientBootstrapClaimRedeemResponse, ClientIdentityMaterial, RelayMode, RendezvousClientConfig,
    RendezvousControlClient, RendezvousEndpointConnectionState, RendezvousEndpointStatus,
    RendezvousRuntimeState, build_signed_request_headers, public_key_fingerprint,
    rendezvous_client_identity_not_after_unix,
};
