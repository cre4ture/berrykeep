import AppleCore
@preconcurrency import FileProvider
import Foundation
import UniformTypeIdentifiers

extension IronmeshFileProviderExtensionHost {
    public func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, (any Error)?) -> Void
    ) -> Progress {
        _ = request
        let progress = Progress(totalUnitCount: 1)
        let completion = UncheckedBox(completionHandler)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let item = try self.service.item(for: identifier)
                progress.completedUnitCount = 1
                completion.value(
                    IronmeshFileProviderItem(
                        bridgeItem: item,
                        domainDisplayName: self.service.configuration.domainDisplayName
                    ),
                    nil
                )
            } catch {
                completion.value(nil, asNSError(error))
            }
        }
        return progress
    }

    public func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, (any Error)?) -> Void
    ) -> Progress {
        _ = request
        let progress = Progress(totalUnitCount: -1)
        let completion = UncheckedBox(completionHandler)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let item = try self.service.item(for: itemIdentifier)
                try self.validate(requestedVersion: requestedVersion, for: item)
                let temporaryDirectory = try self.temporaryDirectory()
                let (fileURL, fetchedItem) = try self.service.fetchContents(
                    for: itemIdentifier,
                    item: item,
                    progress: progress,
                    temporaryDirectory: temporaryDirectory
                )
                completion.value(
                    fileURL,
                    IronmeshFileProviderItem(
                        bridgeItem: fetchedItem,
                        domainDisplayName: self.service.configuration.domainDisplayName
                    ),
                    nil
                )
            } catch {
                completion.value(nil, nil, asNSError(error))
            }
        }
        return progress
    }

    #if os(macOS)
    public func fetchPartialContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion,
        request: NSFileProviderRequest,
        minimalRange: NSRange,
        aligningTo alignment: Int,
        options: NSFileProviderFetchContentsOptions,
        completionHandler: @escaping (
            URL?,
            NSFileProviderItem?,
            NSRange,
            NSFileProviderMaterializationFlags,
            (any Error)?
        ) -> Void
    ) -> Progress {
        _ = request
        _ = options
        let progress = Progress(totalUnitCount: -1)
        let completion = UncheckedBox(completionHandler)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let item = try self.service.item(for: itemIdentifier)
                try self.validate(requestedVersion: requestedVersion, for: item)
                let temporaryDirectory = try self.temporaryDirectory()
                let (fileURL, fetchedItem, fetchedRange) = try self.service.fetchPartialContents(
                    for: itemIdentifier,
                    item: item,
                    minimalRange: minimalRange,
                    alignment: alignment,
                    progress: progress,
                    temporaryDirectory: temporaryDirectory
                )
                completion.value(
                    fileURL,
                    IronmeshFileProviderItem(
                        bridgeItem: fetchedItem,
                        domainDisplayName: self.service.configuration.domainDisplayName
                    ),
                    fetchedRange,
                    [],
                    nil
                )
            } catch {
                completion.value(
                    nil,
                    nil,
                    NSRange(location: NSNotFound, length: 0),
                    [],
                    asNSError(error)
                )
            }
        }
        return progress
    }
    #endif

    private func temporaryDirectory() throws -> URL {
        guard let manager = NSFileProviderManager(for: domain) else {
            throw providerDomainUnavailableError()
        }
        return try manager.temporaryDirectoryURL()
    }

    private func validate(
        requestedVersion: NSFileProviderItemVersion?,
        for item: AppleBridgeItem
    ) throws {
        guard let requestedVersion else {
            return
        }
        let providedVersion = IronmeshFileProviderItem(
            bridgeItem: item,
            domainDisplayName: service.configuration.domainDisplayName
        ).itemVersion
        guard requestedVersion.contentVersion == providedVersion.contentVersion else {
            throw contentVersionUnavailableError()
        }
    }

    public func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> any NSFileProviderEnumerator {
        _ = request
        return IronmeshFileProviderEnumerator(containerIdentifier: containerItemIdentifier, service: service)
    }
}
