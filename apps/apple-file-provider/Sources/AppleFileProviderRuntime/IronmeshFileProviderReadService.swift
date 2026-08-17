import AppleCore
@preconcurrency import FileProvider
import Foundation
import UniformTypeIdentifiers

extension IronmeshFileProviderService {
    func rootItem() -> AppleBridgeItem {
        AppleBridgeItem(
            path: "",
            displayName: configuration.domainDisplayName,
            identifier: .root,
            kind: .directory
        )
    }

    func list(path: String) throws -> [AppleBridgeItem] {
        try connectIfNeeded()
        let items = try bridge.list(path: pathMapper.remotePath(forLocalPath: path), depth: 1)
            .map(pathMapper.localItem)
            .filter { !$0.path.isEmpty }
        cache.record(items: items)
        return items
    }

    func reconcileRemoteChanges(after anchor: UInt64) throws -> (
        itemsByIdentifier: [String: AppleBridgeItem],
        batch: AppleRemoteChangeBatch
    ) {
        try connectIfNeeded()
        let profile = try currentProfile()
        let items = try bridge.list(
            path: pathMapper.remotePath(forLocalPath: ""),
            depth: profile.depth
        )
        .map(pathMapper.localItem)
        .filter { !$0.path.isEmpty }
        cache.record(items: items)
        _ = try changeJournal.reconcile(items)
        let batch = try changeJournal.changes(after: anchor)
        let itemsByIdentifier = items.reduce(into: [String: AppleBridgeItem]()) {
            result, item in
            let identifier = item.identifier.serialized
            if let existing = result[identifier], existing.path <= item.path {
                return
            }
            result[identifier] = item
        }
        return (itemsByIdentifier, batch)
    }

    func currentChangeGeneration() throws -> UInt64 {
        try changeJournal.load().generation
    }

    func activeOriginalShareItems() throws -> [AppleBridgeItem] {
        guard configuration.syncProfile == nil,
              let appGroupIdentifier = configuration.appGroupIdentifier else {
            return []
        }
        return try AppleOriginalShareCapabilityStore(appGroupIdentifier: appGroupIdentifier)
            .activeCapabilities()
            .map { originalShareItem(capability: $0) }
    }

    func item(for identifier: NSFileProviderItemIdentifier) throws -> AppleBridgeItem {
        if identifier == .rootContainer {
            return rootItem()
        }

        let appleIdentifier = try appleIdentifier(from: identifier)
        if let token = appleIdentifier.originalShareToken {
            return try originalShareItem(token: token)
        }

        try connectIfNeeded()
        let lookupPath = try pathForLookup(identifier: appleIdentifier)
        let remoteLookupPath = pathMapper.remotePath(forLocalPath: lookupPath)
        let item = try bridge.metadata(pathOrIdentifier: remoteLookupPath + (appleIdentifier.kind == .directory && !remoteLookupPath.hasSuffix("/") ? "/" : ""))
            ?? { throw fileProviderError(.noSuchItem) }()
        let localItem = try pathMapper.localItem(from: item)
        cache.record(items: [localItem])
        return localItem
    }

    func fetchContents(
        for identifier: NSFileProviderItemIdentifier,
        item: AppleBridgeItem,
        progress: Progress,
        temporaryDirectory: URL
    ) throws -> (URL, AppleBridgeItem) {
        let selection = try contentSelection(for: identifier, item: item)
        let fileURL = temporaryFileURL(
            in: temporaryDirectory,
            displayName: selection.item.displayName
        )
        do {
            try streamSelection(
                selection,
                range: 0..<selection.sizeBytes,
                to: fileURL,
                sparseFileSize: nil,
                progress: progress
            )
            return (fileURL, selection.item)
        } catch {
            try? FileManager.default.removeItem(at: fileURL)
            throw error
        }
    }

    func fetchPartialContents(
        for identifier: NSFileProviderItemIdentifier,
        item: AppleBridgeItem,
        minimalRange: NSRange,
        alignment: Int,
        progress: Progress,
        temporaryDirectory: URL
    ) throws -> (URL, AppleBridgeItem, NSRange) {
        let selection = try contentSelection(for: identifier, item: item)
        let range = try alignedRange(
            minimalRange,
            alignment: alignment,
            fileSize: selection.sizeBytes
        )
        let fileURL = temporaryFileURL(
            in: temporaryDirectory,
            displayName: selection.item.displayName
        )
        do {
            try streamSelection(
                selection,
                range: range,
                to: fileURL,
                sparseFileSize: selection.sizeBytes,
                progress: progress
            )
            guard let location = Int(exactly: range.lowerBound),
                  let length = Int(exactly: range.byteCount) else {
                throw CocoaError(.fileReadTooLarge)
            }
            return (fileURL, selection.item, NSRange(location: location, length: length))
        } catch {
            try? FileManager.default.removeItem(at: fileURL)
            throw error
        }
    }

    private func originalShareItem(token: String) throws -> AppleBridgeItem {
        let capability: AppleOriginalShareCapability
        do {
            capability = try originalShareCapabilityStore().resolve(token: token)
        } catch {
            throw fileProviderError(.noSuchItem)
        }
        return originalShareItem(capability: capability)
    }

    private func originalShareItem(
        capability: AppleOriginalShareCapability
    ) -> AppleBridgeItem {
        AppleBridgeItem(
            path: capability.remotePath,
            displayName: capability.displayName,
            identifier: .originalShare(token: capability.token),
            kind: .file,
            revisionHint: capability.selectorRevision,
            mimeType: capability.mimeType,
            sizeBytes: capability.sizeBytes
        )
    }

    private func contentSelection(
        for identifier: NSFileProviderItemIdentifier,
        item: AppleBridgeItem
    ) throws -> IronmeshContentSelection {
        let appleIdentifier = try appleIdentifier(from: identifier)
        if let token = appleIdentifier.originalShareToken {
            let capability: AppleOriginalShareCapability
            do {
                capability = try originalShareCapabilityStore().resolve(token: token)
            } catch {
                throw fileProviderError(.noSuchItem)
            }
            try connectIfNeeded()
            let size = try bridge.objectSize(
                path: capability.remotePath,
                snapshot: capability.snapshotID,
                version: capability.versionID
            )
            if let expectedSize = capability.sizeBytes,
               UInt64(expectedSize) != size {
                throw fileProviderError(.versionNoLongerAvailable)
            }
            return IronmeshContentSelection(
                item: item,
                remotePath: capability.remotePath,
                snapshotID: capability.snapshotID,
                versionID: capability.versionID,
                sizeBytes: size
            )
        }

        let lookupPath = try pathForLookup(identifier: appleIdentifier)
        let remotePath = pathMapper.remotePath(forLocalPath: lookupPath)
        let size = try bridge.objectSize(
            path: remotePath,
            version: item.revisionHint
        )
        return IronmeshContentSelection(
            item: item,
            remotePath: remotePath,
            snapshotID: nil,
            versionID: item.revisionHint,
            sizeBytes: size
        )
    }

    private func originalShareCapabilityStore() throws -> AppleOriginalShareCapabilityStore {
        guard configuration.syncProfile == nil,
              let appGroupIdentifier = configuration.appGroupIdentifier else {
            throw fileProviderError(.noSuchItem)
        }
        return try AppleOriginalShareCapabilityStore(appGroupIdentifier: appGroupIdentifier)
    }

    private func temporaryFileURL(in directory: URL, displayName: String) -> URL {
        directory.appendingPathComponent(
            "\(UUID().uuidString)-\(displayName)",
            isDirectory: false
        )
    }

    private func streamSelection(
        _ selection: IronmeshContentSelection,
        range: Range<UInt64>,
        to fileURL: URL,
        sparseFileSize: UInt64?,
        progress: Progress
    ) throws {
        guard FileManager.default.createFile(atPath: fileURL.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown)
        }
        let file = try FileHandle(forWritingTo: fileURL)
        defer { try? file.close() }
        if let sparseFileSize {
            try file.truncate(atOffset: sparseFileSize)
        }
        try file.seek(toOffset: range.lowerBound)

        let rangeLength = range.byteCount
        progress.totalUnitCount = Int64(clamping: rangeLength)
        progress.completedUnitCount = 0
        var offset = range.lowerBound
        while offset < range.upperBound {
            try throwIfCancelled(progress)
            let length = Int(min(
                UInt64(IronmeshContentSelection.maximumRangeBytes),
                range.upperBound - offset
            ))
            let bytes = try bridge.downloadRange(
                path: selection.remotePath,
                offset: offset,
                length: length,
                snapshot: selection.snapshotID,
                version: selection.versionID
            )
            guard bytes.count == length else {
                throw CocoaError(.fileReadCorruptFile)
            }
            try file.write(contentsOf: bytes)
            offset += UInt64(length)
            progress.completedUnitCount = Int64(clamping: offset - range.lowerBound)
        }
        try throwIfCancelled(progress)
    }

    private func alignedRange(
        _ minimalRange: NSRange,
        alignment: Int,
        fileSize: UInt64
    ) throws -> Range<UInt64> {
        let requestedEndResult = minimalRange.location.addingReportingOverflow(minimalRange.length)
        guard minimalRange.location != NSNotFound,
              minimalRange.location >= 0,
              minimalRange.length >= 0,
              !requestedEndResult.overflow else {
            throw CocoaError(.fileReadInvalidFileName)
        }
        let requestedEnd = requestedEndResult.partialValue
        let requestedStart = UInt64(minimalRange.location)
        let clampedEnd = min(UInt64(requestedEnd), fileSize)
        guard requestedStart < clampedEnd else {
            throw CocoaError(.fileReadTooLarge)
        }
        let alignment = UInt64(max(alignment, 1))
        let alignedStart = (requestedStart / alignment) * alignment
        let roundedEnd = clampedEnd.addingReportingOverflow(alignment - 1)
        let alignedEnd = roundedEnd.overflow
            ? fileSize
            : min((roundedEnd.partialValue / alignment) * alignment, fileSize)
        return alignedStart..<max(alignedEnd, clampedEnd)
    }

    private func throwIfCancelled(_ progress: Progress) throws {
        guard !progress.isCancelled else {
            throw NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)
        }
    }
}

private struct IronmeshContentSelection {
    static let maximumRangeBytes = 4 * 1_024 * 1_024

    let item: AppleBridgeItem
    let remotePath: String
    let snapshotID: String?
    let versionID: String?
    let sizeBytes: UInt64
}

private extension Range where Bound == UInt64 {
    var byteCount: UInt64 {
        upperBound - lowerBound
    }
}
