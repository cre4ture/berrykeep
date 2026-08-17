import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

public struct AppleOriginalShareRequest: Equatable, Sendable {
    public let requestID: String
    public let remotePath: String
    public let snapshotID: String?
    public let versionID: String?
    public let displayName: String
    public let mimeType: String
    public let sizeBytes: Int64?

    public static func decodeWebMessage(_ data: Data) throws -> Self {
        guard data.count <= AppleOriginalShareLimits.maximumWebMessageBytes else {
            throw AppleOriginalShareError.invalidRequest("The iOS share request is too large.")
        }
        let envelope: WebMessage
        do {
            envelope = try JSONDecoder().decode(WebMessage.self, from: data)
        } catch {
            throw AppleOriginalShareError.invalidRequest("The iOS share request is not valid JSON.")
        }
        guard envelope.action == "share-original" else {
            throw AppleOriginalShareError.invalidRequest("The iOS share action is unsupported.")
        }

        let requestID = try validatedRequiredString(
            envelope.requestID,
            label: "request ID",
            maximumLength: AppleOriginalShareLimits.maximumRequestIDLength
        )
        let remotePath = try validatedRequiredString(
            envelope.key,
            label: "remote path",
            maximumLength: AppleOriginalShareLimits.maximumRemotePathLength
        ).trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        guard !remotePath.isEmpty, !remotePath.contains("\0") else {
            throw AppleOriginalShareError.invalidRequest("The remote path is invalid.")
        }
        let snapshotID = try validatedOptionalString(
            envelope.snapshotID,
            label: "snapshot selector",
            maximumLength: AppleOriginalShareLimits.maximumSelectorLength
        )
        let versionID = try validatedOptionalString(
            envelope.versionID,
            label: "version selector",
            maximumLength: AppleOriginalShareLimits.maximumSelectorLength
        )
        guard (snapshotID == nil) != (versionID == nil) else {
            throw AppleOriginalShareError.invalidRequest(
                "Exactly one immutable snapshot or version selector is required."
            )
        }
        if let sizeBytes = envelope.sizeBytes, sizeBytes < 0 {
            throw AppleOriginalShareError.invalidRequest("The original file size must not be negative.")
        }

        self.init(
            requestID: requestID,
            remotePath: remotePath,
            snapshotID: snapshotID,
            versionID: versionID,
            displayName: sanitizedOriginalShareFilename(
                envelope.fileName.nilIfBlank ?? remotePath.lastPathComponentOrFallback
            ),
            mimeType: normalizedOriginalShareMimeType(envelope.mimeType),
            sizeBytes: envelope.sizeBytes
        )
    }

    private init(
        requestID: String,
        remotePath: String,
        snapshotID: String?,
        versionID: String?,
        displayName: String,
        mimeType: String,
        sizeBytes: Int64?
    ) {
        self.requestID = requestID
        self.remotePath = remotePath
        self.snapshotID = snapshotID
        self.versionID = versionID
        self.displayName = displayName
        self.mimeType = mimeType
        self.sizeBytes = sizeBytes
    }
}

public struct AppleOriginalShareCapability: Codable, Equatable, Sendable {
    public let token: String
    public let remotePath: String
    public let snapshotID: String?
    public let versionID: String?
    public let displayName: String
    public let mimeType: String
    public let sizeBytes: Int64?
    public let createdAt: Date
    public let expiresAt: Date

    public var selectorRevision: String {
        snapshotID.map { "snapshot:\($0)" } ?? "version:\(versionID ?? "invalid")"
    }
}

public final class AppleOriginalShareCapabilityStore: @unchecked Sendable {
    public static let directoryName = "IronmeshOriginalShareCapabilities"

    private static let processLock = NSLock()
    private let directoryURL: URL
    private let clock: () -> Date
    private let tokenFactory: () -> String

    public convenience init(appGroupIdentifier: String) throws {
        #if canImport(Darwin)
        guard let containerURL = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupIdentifier
        ) else {
            throw AppleOriginalShareError.storageUnavailable
        }
        self.init(directoryURL: containerURL.appendingPathComponent(Self.directoryName, isDirectory: true))
        #else
        _ = appGroupIdentifier
        throw AppleOriginalShareError.storageUnavailable
        #endif
    }

    public init(
        directoryURL: URL,
        clock: @escaping () -> Date = Date.init,
        tokenFactory: @escaping () -> String = { UUID().uuidString.lowercased() }
    ) {
        self.directoryURL = directoryURL
        self.clock = clock
        self.tokenFactory = tokenFactory
    }

    public func create(_ request: AppleOriginalShareRequest) throws -> AppleOriginalShareCapability {
        try withExclusiveStoreAccess {
            let now = clock()
            try pruneLocked(now: now, reserving: 1)
            let token = tokenFactory().lowercased()
            guard isValidOriginalShareToken(token) else {
                throw AppleOriginalShareError.invalidToken
            }
            let capability = AppleOriginalShareCapability(
                token: token,
                remotePath: request.remotePath,
                snapshotID: request.snapshotID,
                versionID: request.versionID,
                displayName: request.displayName,
                mimeType: request.mimeType,
                sizeBytes: request.sizeBytes,
                createdAt: now,
                expiresAt: now.addingTimeInterval(AppleOriginalShareLimits.capabilityLifetime)
            )
            try persistLocked(capability)
            return capability
        }
    }

    public func resolve(token: String) throws -> AppleOriginalShareCapability {
        try withExclusiveStoreAccess {
            let normalizedToken = token.lowercased()
            guard isValidOriginalShareToken(normalizedToken) else {
                throw AppleOriginalShareError.invalidToken
            }
            let fileURL = capabilityURL(token: normalizedToken)
            let capability: AppleOriginalShareCapability
            do {
                capability = try JSONDecoder().decode(
                    AppleOriginalShareCapability.self,
                    from: Data(contentsOf: fileURL)
                )
            } catch {
                try? FileManager.default.removeItem(at: fileURL)
                throw AppleOriginalShareError.capabilityUnavailable
            }
            guard capability.token == normalizedToken,
                  isValidStoredCapability(capability) else {
                try? FileManager.default.removeItem(at: fileURL)
                throw AppleOriginalShareError.capabilityUnavailable
            }
            guard capability.expiresAt > clock() else {
                try? FileManager.default.removeItem(at: fileURL)
                throw AppleOriginalShareError.capabilityExpired
            }
            return capability
        }
    }

    public func remove(token: String) {
        try? withExclusiveStoreAccess {
            guard isValidOriginalShareToken(token.lowercased()) else {
                return
            }
            try? FileManager.default.removeItem(at: capabilityURL(token: token.lowercased()))
        }
    }

    public func activeCapabilities() throws -> [AppleOriginalShareCapability] {
        try withExclusiveStoreAccess {
            let now = clock()
            try pruneLocked(now: now, reserving: 0)
            return try FileManager.default.contentsOfDirectory(
                at: directoryURL,
                includingPropertiesForKeys: nil,
                options: [.skipsHiddenFiles]
            )
            .filter { $0.pathExtension == "json" }
            .compactMap { fileURL in
                guard let data = try? Data(contentsOf: fileURL),
                      let capability = try? JSONDecoder().decode(
                        AppleOriginalShareCapability.self,
                        from: data
                      ),
                      isValidStoredCapability(capability),
                      capability.expiresAt > now else {
                    return nil
                }
                return capability
            }
            .sorted { $0.createdAt > $1.createdAt }
        }
    }

    private func withExclusiveStoreAccess<T>(_ operation: () throws -> T) throws -> T {
        // Store instances are short-lived. The static lock coordinates them within one process;
        // flock coordinates the host app and File Provider extension through the app-group file.
        Self.processLock.lock()
        defer { Self.processLock.unlock() }

        try ensureDirectoryExists()
        let lockFileURL = directoryURL.appendingPathComponent(".store.lock", isDirectory: false)
        _ = FileManager.default.createFile(atPath: lockFileURL.path, contents: Data())
        let lockFile: FileHandle
        do {
            lockFile = try FileHandle(forUpdating: lockFileURL)
        } catch {
            throw AppleOriginalShareError.storageUnavailable
        }
        defer { try? lockFile.close() }

        #if canImport(Darwin) || canImport(Glibc)
        while flock(lockFile.fileDescriptor, LOCK_EX) != 0 {
            guard errno == EINTR else {
                throw AppleOriginalShareError.storageUnavailable
            }
        }
        defer { _ = flock(lockFile.fileDescriptor, LOCK_UN) }
        #else
        throw AppleOriginalShareError.storageUnavailable
        #endif

        return try operation()
    }

    private func ensureDirectoryExists() throws {
        do {
            try FileManager.default.createDirectory(
                at: directoryURL,
                withIntermediateDirectories: true
            )
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            var mutableDirectoryURL = directoryURL
            try mutableDirectoryURL.setResourceValues(values)
        } catch {
            throw AppleOriginalShareError.storageUnavailable
        }
    }

    private func pruneLocked(now: Date, reserving: Int) throws {
        let fileURLs = try FileManager.default.contentsOfDirectory(
            at: directoryURL,
            includingPropertiesForKeys: nil,
            options: [.skipsHiddenFiles]
        ).filter { $0.pathExtension == "json" }
        let active = fileURLs.compactMap { fileURL -> AppleOriginalShareCapability? in
            guard let data = try? Data(contentsOf: fileURL),
                  let capability = try? JSONDecoder().decode(AppleOriginalShareCapability.self, from: data),
                  isValidStoredCapability(capability),
                  capability.expiresAt > now else {
                try? FileManager.default.removeItem(at: fileURL)
                return nil
            }
            return capability
        }
        let retainedCount = max(AppleOriginalShareLimits.maximumCapabilityCount - reserving, 0)
        for capability in active.sorted(by: { $0.createdAt > $1.createdAt }).dropFirst(retainedCount) {
            try? FileManager.default.removeItem(at: capabilityURL(token: capability.token))
        }
    }

    private func persistLocked(_ capability: AppleOriginalShareCapability) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        do {
            let fileURL = capabilityURL(token: capability.token)
            try encoder.encode(capability).write(to: fileURL, options: [.atomic])
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            var mutableFileURL = fileURL
            try mutableFileURL.setResourceValues(values)
        } catch {
            throw AppleOriginalShareError.storageUnavailable
        }
    }

    private func capabilityURL(token: String) -> URL {
        directoryURL.appendingPathComponent("\(token).json", isDirectory: false)
    }
}

public enum AppleOriginalShareError: LocalizedError, Equatable {
    case invalidRequest(String)
    case invalidToken
    case capabilityUnavailable
    case capabilityExpired
    case storageUnavailable

    public var errorDescription: String? {
        switch self {
        case .invalidRequest(let message):
            message
        case .invalidToken:
            "The original-share capability is invalid."
        case .capabilityUnavailable:
            "The original-share capability is unavailable."
        case .capabilityExpired:
            "The original-share capability has expired."
        case .storageUnavailable:
            "The original-share capability could not be stored."
        }
    }
}

private struct WebMessage: Decodable {
    let action: String
    let requestID: String
    let key: String
    let snapshotID: String?
    let versionID: String?
    let fileName: String?
    let mimeType: String?
    let sizeBytes: Int64?

    enum CodingKeys: String, CodingKey {
        case action
        case requestID = "requestId"
        case key
        case snapshotID = "snapshotId"
        case versionID = "versionId"
        case fileName
        case mimeType
        case sizeBytes
    }
}

private enum AppleOriginalShareLimits {
    static let maximumWebMessageBytes = 64 * 1_024
    static let maximumRequestIDLength = 128
    static let maximumRemotePathLength = 4_096
    static let maximumSelectorLength = 1_024
    static let maximumMimeTypeLength = 255
    static let capabilityLifetime: TimeInterval = 24 * 60 * 60
    static let maximumCapabilityCount = 64
}

private func validatedRequiredString(
    _ value: String,
    label: String,
    maximumLength: Int
) throws -> String {
    guard let value = value.nilIfBlank,
          value.count <= maximumLength,
          !value.contains("\0") else {
        throw AppleOriginalShareError.invalidRequest("The \(label) is invalid.")
    }
    return value
}

private func validatedOptionalString(
    _ value: String?,
    label: String,
    maximumLength: Int
) throws -> String? {
    guard let value = value.nilIfBlank else {
        return nil
    }
    guard value.count <= maximumLength, !value.contains("\0") else {
        throw AppleOriginalShareError.invalidRequest("The \(label) is invalid.")
    }
    return value
}

private func sanitizedOriginalShareFilename(_ candidate: String) -> String {
    let leafName = (candidate as NSString).lastPathComponent
    let invalidCharacters = CharacterSet(charactersIn: "<>:\"/\\|?*")
        .union(.controlCharacters)
    let sanitized = leafName
        .components(separatedBy: invalidCharacters)
        .joined(separator: "_")
        .trimmingCharacters(in: .whitespacesAndNewlines.union(CharacterSet(charactersIn: ".")))
    return sanitized.nilIfBlank ?? "download.bin"
}

private func normalizedOriginalShareMimeType(_ candidate: String?) -> String {
    guard let candidate = candidate.nilIfBlank,
          candidate.count <= AppleOriginalShareLimits.maximumMimeTypeLength,
          candidate.contains("/"),
          !candidate.contains(where: { $0.isWhitespace || $0.isNewline }) else {
        return "application/octet-stream"
    }
    return candidate.lowercased()
}

private func isValidOriginalShareToken(_ token: String) -> Bool {
    guard token.count == 36,
          let uuid = UUID(uuidString: token) else {
        return false
    }
    return uuid.uuidString.lowercased() == token.lowercased()
}

private func isValidStoredCapability(_ capability: AppleOriginalShareCapability) -> Bool {
    isValidOriginalShareToken(capability.token)
        && !capability.remotePath.isEmpty
        && capability.remotePath.count <= AppleOriginalShareLimits.maximumRemotePathLength
        && !capability.remotePath.contains("\0")
        && ((capability.snapshotID == nil) != (capability.versionID == nil))
        && (capability.snapshotID?.count ?? 0) <= AppleOriginalShareLimits.maximumSelectorLength
        && (capability.versionID?.count ?? 0) <= AppleOriginalShareLimits.maximumSelectorLength
        && capability.displayName == sanitizedOriginalShareFilename(capability.displayName)
        && capability.mimeType == normalizedOriginalShareMimeType(capability.mimeType)
        && (capability.sizeBytes == nil || capability.sizeBytes! >= 0)
        && capability.expiresAt > capability.createdAt
}
