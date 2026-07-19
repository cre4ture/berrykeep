import AppleCore
@preconcurrency import FileProvider
import Foundation
import Network

protocol IronmeshSyncEnvironmentProviding: Sendable {
    func snapshot() -> AppleSyncEnvironmentSnapshot
}

final class IronmeshLiveSyncEnvironment: IronmeshSyncEnvironmentProviding, @unchecked Sendable {
    static let shared = IronmeshLiveSyncEnvironment()

    private let monitor = NWPathMonitor()
    private let lock = NSLock()
    private var latestPath: NWPath?

    private init() {
        monitor.pathUpdateHandler = { [weak self] path in
            self?.lock.lock()
            self?.latestPath = path
            self?.lock.unlock()
        }
        monitor.start(queue: DispatchQueue(label: "dev.ironmesh.apple.sync-network-path"))
    }

    func snapshot() -> AppleSyncEnvironmentSnapshot {
        lock.lock()
        let path = latestPath ?? monitor.currentPath
        lock.unlock()
        return AppleSyncEnvironmentSnapshot(
            isConnected: path.status == .satisfied,
            isExpensive: path.isExpensive,
            isConstrained: path.isConstrained,
            isLowPowerModeEnabled: ProcessInfo.processInfo.isLowPowerModeEnabled
        )
    }
}

enum IronmeshSyncAnchorCodec {
    static func generation(from anchor: NSFileProviderSyncAnchor) throws -> UInt64 {
        guard !anchor.rawValue.isEmpty else {
            return 0
        }
        guard let value = String(data: anchor.rawValue, encoding: .utf8),
              let generation = UInt64(value) else {
            throw NSError(
                domain: NSFileProviderErrorDomain,
                code: NSFileProviderError.Code.syncAnchorExpired.rawValue
            )
        }
        return generation
    }

    static func anchor(for generation: UInt64) -> NSFileProviderSyncAnchor {
        NSFileProviderSyncAnchor(rawValue: Data(String(generation).utf8))
    }
}

func ironmeshChangeJournalURL(domainIdentifier: String) -> URL {
    let supportRoot = FileManager.default.urls(
        for: .applicationSupportDirectory,
        in: .userDomainMask
    ).first ?? FileManager.default.temporaryDirectory
    return supportRoot
        .appendingPathComponent("IronMeshApple", isDirectory: true)
        .appendingPathComponent("ChangeJournals", isDirectory: true)
        .appendingPathComponent("\(domainIdentifier)-changes.json")
}
