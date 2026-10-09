import Foundation

public struct AppleTaskQueueEntry: Codable, Equatable, Sendable, Identifiable {
    public let id: String
    public let label: String
    public let pending: UInt64
    public let active: UInt64
    public let capacity: UInt64?
    public let state: String
    public let detail: String?

    public init(
        id: String,
        label: String,
        pending: UInt64,
        active: UInt64,
        capacity: UInt64? = nil,
        state: String,
        detail: String? = nil
    ) {
        self.id = id
        self.label = label
        self.pending = pending
        self.active = active
        self.capacity = capacity
        self.state = state
        self.detail = detail
    }
}

public struct AppleClusterTaskQueueNodeSnapshot: Codable, Equatable, Sendable {
    public let nodeID: String
    public let queues: [AppleTaskQueueEntry]

    enum CodingKeys: String, CodingKey {
        case nodeID = "node_id"
        case queues
    }
}

public struct AppleUnavailableTaskQueueNode: Codable, Equatable, Sendable {
    public let nodeID: String
    public let error: String

    enum CodingKeys: String, CodingKey {
        case nodeID = "node_id"
        case error
    }
}

public struct AppleClusterTaskQueueSnapshot: Codable, Equatable, Sendable {
    public let generatedAtUnixMs: UInt64
    public let nodes: [AppleClusterTaskQueueNodeSnapshot]
    public let unavailableNodes: [AppleUnavailableTaskQueueNode]

    enum CodingKeys: String, CodingKey {
        case generatedAtUnixMs = "generated_at_unix_ms"
        case nodes
        case unavailableNodes = "unavailable_nodes"
    }
}
