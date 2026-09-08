import AppleCore
import Combine
import Foundation

struct IronmeshGalleryLoadContext: Equatable, Sendable {
    let configuration: AppleConnectionConfiguration
    let query: AppleGalleryQuery
}

@MainActor
final class IronmeshGalleryModel: ObservableObject {
    @Published private(set) var entries: [AppleStoreIndexEntry] = []
    @Published private(set) var totalCount = 0
    @Published private(set) var isLoading = false
    @Published private(set) var errorMessage: String?

    let imageRepository: IronmeshGalleryImageRepository

    private let remoteSession: IronmeshGalleryRemoteSession
    private var activeContext: IronmeshGalleryLoadContext?
    private var activeGeneration: UInt64?
    private var pagination = AppleGalleryPagination()
    private var requestGate = AppleGalleryRequestGate()
    private var pageTask: Task<Void, Never>?

    init(
        remoteSession: IronmeshGalleryRemoteSession = IronmeshGalleryRemoteSession(),
        imageRepository: IronmeshGalleryImageRepository = IronmeshGalleryImageRepository()
    ) {
        self.remoteSession = remoteSession
        self.imageRepository = imageRepository
    }

    var canLoadMore: Bool {
        pagination.hasMore && entries.count < totalCount
    }

    func reload(
        mode: AppleGalleryMode,
        sort: AppleGallerySort,
        currentPath: String,
        configuration: AppleConnectionConfiguration?,
        showSensitiveContent: Bool = false,
        captureDateRange: AppleGalleryCaptureDateRange = AppleGalleryCaptureDateRange(),
        force: Bool = false
    ) {
        guard let configuration else {
            resetWithError("Configure a connection before loading photos.")
            return
        }

        let context = IronmeshGalleryLoadContext(
            configuration: configuration,
            query: AppleGalleryQuery(
                mode: mode,
                currentPath: currentPath,
                sort: sort,
                showSensitiveContent: showSensitiveContent,
                captureDateRange: captureDateRange
            )
        )
        guard force || context != activeContext else {
            return
        }

        pageTask?.cancel()
        let generation = requestGate.begin()
        activeContext = context
        activeGeneration = generation
        pagination = AppleGalleryPagination()
        entries = []
        totalCount = 0
        errorMessage = nil
        imageRepository.prepare(for: configuration)
        loadPage(context: context, generation: generation, offset: 0)
    }

    func refresh() {
        guard let activeContext else {
            return
        }
        reload(
            mode: activeContext.query.mode,
            sort: activeContext.query.sort,
            currentPath: activeContext.query.currentPath,
            configuration: activeContext.configuration,
            showSensitiveContent: activeContext.query.showSensitiveContent,
            captureDateRange: activeContext.query.captureDateRange,
            force: true
        )
    }

    func toggleSensitiveLabel(for entry: AppleStoreIndexEntry, label: String) {
        guard let context = activeContext else {
            return
        }
        guard entry.labelsResolved == true else {
            errorMessage = "Image labels are temporarily unavailable. Refresh before editing them."
            return
        }
        let labels = entry.labels ?? []
        let nextLabels = labels.contains(label)
            ? labels.filter { $0 != label }
            : labels + [label]
        let remoteSession = remoteSession
        Task { [weak self] in
            do {
                try await Task.detached(priority: .userInitiated) {
                    try remoteSession.setMediaLabels(
                        path: entry.path,
                        labels: nextLabels,
                        configuration: context.configuration
                    )
                }.value
                guard let self, self.activeContext == context else {
                    return
                }
                self.refresh()
            } catch {
                guard let self, self.activeContext == context else {
                    return
                }
                self.errorMessage = error.localizedDescription
            }
        }
    }

    func loadNextPage() {
        guard
            !isLoading,
            pagination.hasMore,
            let activeContext,
            let activeGeneration,
            requestGate.accepts(activeGeneration)
        else {
            return
        }
        loadPage(
            context: activeContext,
            generation: activeGeneration,
            offset: pagination.nextOffset
        )
    }

    func retry() {
        if entries.isEmpty {
            refresh()
        } else {
            loadNextPage()
        }
    }

    func invalidate() {
        pageTask?.cancel()
        pageTask = nil
        requestGate.invalidate()
        activeGeneration = nil
        isLoading = false
    }

    func suspend() {
        invalidate()
        activeContext = nil
    }

    private func loadPage(
        context: IronmeshGalleryLoadContext,
        generation: UInt64,
        offset: Int
    ) {
        let request = context.query.request(offset: offset)
        let remoteSession = remoteSession
        isLoading = true
        errorMessage = nil

        pageTask = Task { [weak self] in
            do {
                let response = try await Task.detached(priority: .userInitiated) {
                    try remoteSession.storeIndex(request, configuration: context.configuration)
                }.value

                guard
                    let self,
                    self.requestGate.accepts(generation),
                    self.activeContext == context
                else {
                    return
                }

                let imageEntries = response.entries.filter { $0.entryType == .key }
                let existingPaths = Set(self.entries.map(\.path))
                self.entries.append(contentsOf: imageEntries.filter { !existingPaths.contains($0.path) })
                self.pagination.record(response)
                self.totalCount = max(response.totalEntryCount, self.entries.count)
                self.isLoading = false
                self.errorMessage = nil
            } catch {
                guard
                    let self,
                    self.requestGate.accepts(generation),
                    self.activeContext == context
                else {
                    return
                }
                self.isLoading = false
                self.errorMessage = error.localizedDescription
            }
        }
    }

    private func resetWithError(_ message: String) {
        invalidate()
        activeContext = nil
        entries = []
        totalCount = 0
        pagination = AppleGalleryPagination()
        errorMessage = message
    }
}

final class IronmeshGalleryImageRepository: @unchecked Sendable {
    private let thumbnailCache = NSCache<NSString, NSData>()
    private let viewerPreviewCache = NSCache<NSString, NSData>()
    private let fullImageCache = NSCache<NSString, NSData>()
    private let thumbnailSessions: [IronmeshGalleryRemoteSession]
    private let fullImageSession: IronmeshGalleryRemoteSession
    private let cacheContextLock = NSLock()
    private var cacheContextGate = AppleGalleryCacheContextGate()
    private let thumbnailSessionPool: IronmeshGalleryThumbnailSessionPool

    init(
        thumbnailSessions: [IronmeshGalleryRemoteSession]? = nil,
        fullImageSession: IronmeshGalleryRemoteSession = IronmeshGalleryRemoteSession()
    ) {
        let resolvedThumbnailSessions = thumbnailSessions?.isEmpty == false
            ? thumbnailSessions!
            : (0..<4).map { _ in IronmeshGalleryRemoteSession() }
        self.thumbnailSessions = resolvedThumbnailSessions
        self.fullImageSession = fullImageSession
        thumbnailSessionPool = IronmeshGalleryThumbnailSessionPool(
            sessionCount: resolvedThumbnailSessions.count
        )
        thumbnailCache.countLimit = 160
        thumbnailCache.totalCostLimit = 48 * 1_024 * 1_024
        viewerPreviewCache.countLimit = 8
        viewerPreviewCache.totalCostLimit = 48 * 1_024 * 1_024
        fullImageCache.countLimit = 4
        fullImageCache.totalCostLimit = 96 * 1_024 * 1_024
    }

    func prepare(for configuration: AppleConnectionConfiguration) {
        cacheContextLock.lock()
        defer { cacheContextLock.unlock() }
        let preparation = cacheContextGate.prepare(for: configuration)
        guard preparation.contextChanged else {
            return
        }
        thumbnailCache.removeAllObjects()
        viewerPreviewCache.removeAllObjects()
        fullImageCache.removeAllObjects()
    }

    func thumbnailData(
        for entry: AppleStoreIndexEntry,
        configuration: AppleConnectionConfiguration
    ) async throws -> Data {
        try await thumbnailData(
            for: entry,
            configuration: configuration,
            profile: .grid,
            cache: thumbnailCache,
            cacheKey: AppleGalleryCacheIdentity.thumbnailKey(for: entry),
            priority: .utility
        )
    }

    func viewerPreviewData(
        for entry: AppleStoreIndexEntry,
        configuration: AppleConnectionConfiguration
    ) async throws -> Data {
        try await thumbnailData(
            for: entry,
            configuration: configuration,
            profile: .mobileViewer,
            cache: viewerPreviewCache,
            cacheKey: AppleGalleryCacheIdentity.fullImageKey(for: entry),
            priority: .userInitiated
        )
    }

    private func thumbnailData(
        for entry: AppleStoreIndexEntry,
        configuration: AppleConnectionConfiguration,
        profile: AppleGalleryThumbnailProfile,
        cache: NSCache<NSString, NSData>,
        cacheKey: String,
        priority: TaskPriority
    ) async throws -> Data {
        let lookup = try cacheLookup(
            cache: cache,
            key: cacheKey,
            configuration: configuration
        )
        if let data = lookup.data {
            return data
        }

        let relativePath = AppleGalleryThumbnailPath.relativePath(for: entry, profile: profile)
        let thumbnailSessions = thumbnailSessions
        let data = try await thumbnailSessionPool.perform(priority: priority) { sessionIndex in
            let thumbnailSession = thumbnailSessions[sessionIndex]
            try await Task.detached(priority: priority) {
                try thumbnailSession.fetchRelativeBytes(
                    path: relativePath,
                    configuration: configuration
                )
            }.value
        }

        try storeCacheResult(
            data,
            cache: cache,
            key: cacheKey,
            generation: lookup.generation,
            configuration: configuration
        )
        return data
    }

    func fullImageData(
        for entry: AppleStoreIndexEntry,
        configuration: AppleConnectionConfiguration
    ) async throws -> Data {
        let cacheKey = AppleGalleryCacheIdentity.fullImageKey(for: entry)
        let lookup = try cacheLookup(
            cache: fullImageCache,
            key: cacheKey,
            configuration: configuration
        )
        if let data = lookup.data {
            return data
        }

        let fullImageSession = fullImageSession
        let data = try await Task.detached(priority: .userInitiated) {
            try fullImageSession.download(path: entry.path, configuration: configuration)
        }.value

        try storeCacheResult(
            data,
            cache: fullImageCache,
            key: cacheKey,
            generation: lookup.generation,
            configuration: configuration
        )
        return data
    }

    private func cacheLookup(
        cache: NSCache<NSString, NSData>,
        key: String,
        configuration: AppleConnectionConfiguration
    ) throws -> (generation: UInt64, data: Data?) {
        cacheContextLock.lock()
        defer { cacheContextLock.unlock() }
        guard let generation = cacheContextGate.generation(for: configuration) else {
            throw IronmeshGalleryImageRepositoryError.staleConnectionContext
        }
        let data = cache.object(forKey: key as NSString).map(Data.init(referencing:))
        return (generation, data)
    }

    private func storeCacheResult(
        _ data: Data,
        cache: NSCache<NSString, NSData>,
        key: String,
        generation: UInt64,
        configuration: AppleConnectionConfiguration
    ) throws {
        cacheContextLock.lock()
        defer { cacheContextLock.unlock() }
        guard cacheContextGate.accepts(generation: generation, configuration: configuration) else {
            throw IronmeshGalleryImageRepositoryError.staleConnectionContext
        }
        cache.setObject(data as NSData, forKey: key as NSString, cost: data.count)
    }

}

private actor IronmeshGalleryThumbnailSessionPool {
    private struct Waiter {
        let id: UUID
        let priority: TaskPriority
        let continuation: CheckedContinuation<Int, Error>
    }

    private var availableSessionIndices: [Int]
    private var waiters: [Waiter] = []

    init(sessionCount: Int) {
        precondition(sessionCount > 0)
        availableSessionIndices = Array(0..<sessionCount)
    }

    func perform<T: Sendable>(
        priority: TaskPriority,
        _ operation: @Sendable (Int) async throws -> T
    ) async throws -> T {
        try Task.checkCancellation()
        let sessionIndex = try await acquire(priority: priority)
        defer { release(sessionIndex) }
        try Task.checkCancellation()
        return try await operation(sessionIndex)
    }

    private func acquire(priority: TaskPriority) async throws -> Int {
        if let sessionIndex = availableSessionIndices.popLast() {
            return sessionIndex
        }

        let waiterID = UUID()
        try await withTaskCancellationHandler {
                try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Int, Error>) in
                if Task.isCancelled {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                waiters.append(
                    Waiter(id: waiterID, priority: priority, continuation: continuation)
                )
            }
        } onCancel: {
            Task {
                await self.cancelWaiter(id: waiterID)
            }
        }
    }

    private func release(_ sessionIndex: Int) {
        if let index = waiters.indices.max(by: {
            waiters[$0].priority.rawValue < waiters[$1].priority.rawValue
        }) {
            let waiter = waiters.remove(at: index)
            waiter.continuation.resume(returning: sessionIndex)
        } else {
            availableSessionIndices.append(sessionIndex)
        }
    }

    private func cancelWaiter(id: UUID) {
        guard let index = waiters.firstIndex(where: { $0.id == id }) else {
            return
        }
        let waiter = waiters.remove(at: index)
        waiter.continuation.resume(throwing: CancellationError())
    }
}

private enum IronmeshGalleryImageRepositoryError: LocalizedError {
    case staleConnectionContext

    var errorDescription: String? {
        "The gallery connection changed before the image finished loading."
    }
}

final class IronmeshGalleryRemoteSession: @unchecked Sendable {
    private let bridge: AppleCFacadeBridge
    private let lock = NSLock()
    private var configuration: AppleConnectionConfiguration?

    init(ffi: AppleManualCBridgeFFI = IronmeshRustFFIAdapter(connectionName: "ios gallery")) {
        bridge = AppleCFacadeBridge(ffi: ffi)
    }

    func storeIndex(
        _ request: AppleStoreIndexRequest,
        configuration: AppleConnectionConfiguration
    ) throws -> AppleStoreIndexResponse {
        return try withBridge(configuration: configuration) { bridge in
            try bridge.storeIndex(request)
        }
    }

    func fetchRelativeBytes(
        path: String,
        configuration: AppleConnectionConfiguration
    ) throws -> Data {
        return try withBridge(configuration: configuration) { bridge in
            try bridge.fetchRelativeBytes(path: path)
        }
    }

    func download(
        path: String,
        configuration: AppleConnectionConfiguration
    ) throws -> Data {
        return try withBridge(configuration: configuration) { bridge in
            try bridge.download(path: path, revisionHint: nil)
        }
    }

    func setMediaLabels(
        path: String,
        labels: [String],
        configuration: AppleConnectionConfiguration
    ) throws {
        try withBridge(configuration: configuration) { bridge in
            try bridge.setMediaLabels(path: path, labels: labels)
        }
    }

    private func withBridge<T>(
        configuration nextConfiguration: AppleConnectionConfiguration,
        operation: (AppleCFacadeBridge) throws -> T
    ) throws -> T {
        lock.lock()
        defer { lock.unlock() }

        if configuration != nextConfiguration {
            _ = try bridge.connect(nextConfiguration)
            configuration = nextConfiguration
        }
        return try operation(bridge)
    }
}
