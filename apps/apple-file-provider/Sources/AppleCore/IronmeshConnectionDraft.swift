import Foundation

public let appleMinimumNodeConnectionPriority = -20
public let appleMaximumNodeConnectionPriority = 20

public enum AppleNodePriorityOverrideError: LocalizedError, Equatable {
    case invalidBootstrap
    case invalidNodeID
    case priorityOutOfRange

    public var errorDescription: String? {
        switch self {
        case .invalidBootstrap:
            return "The connection bootstrap is not valid JSON."
        case .invalidNodeID:
            return "The server node ID is invalid."
        case .priorityOutOfRange:
            return "Server node priority must be between \(appleMinimumNodeConnectionPriority) and \(appleMaximumNodeConnectionPriority)."
        }
    }
}

public struct IronmeshConnectionDraft: Codable, Equatable, Sendable {
    public var deviceLabel: String
    public var bootstrapInput: String
    public var serverCAPem: String
    public var clientIdentityJSON: String
    public var enrolledDeviceID: String
    public var domainIdentifier: String
    public var domainDisplayName: String

    public init(
        deviceLabel: String = "",
        bootstrapInput: String = "",
        serverCAPem: String = "",
        clientIdentityJSON: String = "",
        enrolledDeviceID: String = "",
        domainIdentifier: String = "dev.ironmesh.default",
        domainDisplayName: String = "BerryKeep"
    ) {
        self.deviceLabel = deviceLabel
        self.bootstrapInput = bootstrapInput
        self.serverCAPem = serverCAPem
        self.clientIdentityJSON = clientIdentityJSON
        self.enrolledDeviceID = enrolledDeviceID
        self.domainIdentifier = domainIdentifier
        self.domainDisplayName = domainDisplayName
    }

    private enum CodingKeys: String, CodingKey {
        case deviceLabel
        case bootstrapInput
        case serverCAPem
        case clientIdentityJSON
        case enrolledDeviceID
        case domainIdentifier
        case domainDisplayName
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.init(
            deviceLabel: try container.decodeIfPresent(
                String.self,
                forKey: .deviceLabel
            ) ?? "",
            bootstrapInput: try container.decodeIfPresent(
                String.self,
                forKey: .bootstrapInput
            ) ?? "",
            serverCAPem: try container.decodeIfPresent(
                String.self,
                forKey: .serverCAPem
            ) ?? "",
            clientIdentityJSON: try container.decodeIfPresent(
                String.self,
                forKey: .clientIdentityJSON
            ) ?? "",
            enrolledDeviceID: try container.decodeIfPresent(
                String.self,
                forKey: .enrolledDeviceID
            ) ?? "",
            domainIdentifier: try container.decodeIfPresent(
                String.self,
                forKey: .domainIdentifier
            ) ?? "dev.ironmesh.default",
            domainDisplayName: try container.decodeIfPresent(
                String.self,
                forKey: .domainDisplayName
            ) ?? "IronMesh"
        )
    }

    public func encode(to encoder: Encoder) throws {
        // Decode the legacy identity for migration, but never encode it again.
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(deviceLabel, forKey: .deviceLabel)
        try container.encode(bootstrapInput, forKey: .bootstrapInput)
        try container.encode(serverCAPem, forKey: .serverCAPem)
        try container.encode(enrolledDeviceID, forKey: .enrolledDeviceID)
        try container.encode(domainIdentifier, forKey: .domainIdentifier)
        try container.encode(domainDisplayName, forKey: .domainDisplayName)
    }

    public var effectiveConnectionInput: String {
        bootstrapInput.nilIfBlank ?? ""
    }

    public var isConfigured: Bool {
        effectiveConnectionInput.nilIfBlank != nil
    }

    public var hasBootstrapPayload: Bool {
        bootstrapInput.nilIfBlank != nil
    }

    public var hasClientIdentity: Bool {
        clientIdentityJSON.nilIfBlank != nil
    }

    public var hasEnrolledDevice: Bool {
        enrolledDeviceID.nilIfBlank != nil
    }

    public var requiresEnrollment: Bool {
        // A claim is only an enrollment input, never a reconnectable client configuration. Keep
        // a partially persisted claim on the onboarding screen even if a Keychain write from an
        // interrupted enrollment left an identity behind.
        if Self.looksLikeBootstrapClaim(bootstrapInput) {
            return true
        }
        return hasBootstrapPayload && !hasClientIdentity
    }

    public var connectionConfiguration: AppleConnectionConfiguration? {
        guard let connectionInput = effectiveConnectionInput.nilIfBlank else {
            return nil
        }

        return AppleConnectionConfiguration(
            connectionInput: connectionInput,
            serverCAPem: serverCAPem.nilIfBlank,
            clientIdentityJSON: clientIdentityJSON.nilIfBlank
        )
    }

    public var normalizedConnectionInput: String? {
        connectionConfiguration?.normalizedConnectionInput
    }

    public var nodePriorityOverrides: [String: Int] {
        guard let data = effectiveConnectionInput.data(using: .utf8),
              let bootstrap = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let rawOverrides = bootstrap["node_priority_overrides"] as? [String: Any]
        else {
            return [:]
        }
        return rawOverrides.reduce(into: [String: Int]()) { result, entry in
            guard let priority = entry.value as? Int,
                  (appleMinimumNodeConnectionPriority...appleMaximumNodeConnectionPriority)
                    .contains(priority)
            else {
                return
            }
            result[entry.key] = priority
        }
    }

    public mutating func setNodePriorityOverride(_ priority: Int?, for nodeID: String) throws {
        let normalizedNodeID = nodeID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard UUID(uuidString: normalizedNodeID) != nil else {
            throw AppleNodePriorityOverrideError.invalidNodeID
        }
        if let priority,
           !(appleMinimumNodeConnectionPriority...appleMaximumNodeConnectionPriority)
            .contains(priority) {
            throw AppleNodePriorityOverrideError.priorityOutOfRange
        }
        guard let data = effectiveConnectionInput.data(using: .utf8),
              var bootstrap = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            throw AppleNodePriorityOverrideError.invalidBootstrap
        }
        var overrides = bootstrap["node_priority_overrides"] as? [String: Any] ?? [:]
        if let priority {
            overrides[normalizedNodeID] = priority
        } else {
            overrides.removeValue(forKey: normalizedNodeID)
        }
        if overrides.isEmpty {
            bootstrap.removeValue(forKey: "node_priority_overrides")
        } else {
            bootstrap["node_priority_overrides"] = overrides
        }
        let encoded = try JSONSerialization.data(
            withJSONObject: bootstrap,
            options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        )
        guard let updated = String(data: encoded, encoding: .utf8) else {
            throw AppleNodePriorityOverrideError.invalidBootstrap
        }
        bootstrapInput = updated
    }

    public var setupSummary: String {
        if normalizedConnectionInput != nil {
            return "Bootstrap bundle ready for \(domainDisplayName.nilIfBlank ?? "BerryKeep")."
        }

        return "Add a bootstrap bundle to continue."
    }

    public var enrollmentSummary: String {
        if let enrolledDeviceID = enrolledDeviceID.nilIfBlank {
            if let label = deviceLabel.nilIfBlank {
                return "Enrolled as \(label) (\(enrolledDeviceID))."
            }
            return "Enrolled device \(enrolledDeviceID)."
        }

        if hasBootstrapPayload && hasClientIdentity {
            return "Bootstrap bundle imported and client identity attached."
        }
        if hasBootstrapPayload {
            return "Bootstrap bundle imported. Enroll this device to mint client identity material."
        }
        return hasClientIdentity
            ? "Client identity attached. Import a bootstrap bundle to connect."
            : "No bootstrap bundle configured yet."
    }

    public var identitySummary: String {
        if let enrolledDeviceID = enrolledDeviceID.nilIfBlank {
            return "Client identity attached for \(enrolledDeviceID)."
        }
        return hasClientIdentity ? "Client identity attached." : "No client identity attached."
    }

    public func appliedConnectionState(defaultBootstrapInput: String) -> AppleStoredConnectionState {
        AppleStoredConnectionState(
            connectionInput: effectiveConnectionInput.nilIfBlank ?? defaultBootstrapInput,
            serverCAPem: serverCAPem.nilIfBlank,
            clientIdentityJSON: clientIdentityJSON.nilIfBlank,
            deviceID: enrolledDeviceID.nilIfBlank,
            deviceLabel: deviceLabel.nilIfBlank,
            bootstrapInputDraft: bootstrapInput.nilIfBlank
        )
    }

    @discardableResult
    public mutating func applyScannedCode(_ scannedValue: String) -> Bool {
        guard let payload = Self.scannedConnectionPayload(from: scannedValue) else {
            return false
        }

        guard Self.looksLikeBootstrap(payload) else {
            return false
        }

        bootstrapInput = payload
        return true
    }

    public static func scannedConnectionPayload(from scannedValue: String) -> String? {
        let trimmed = scannedValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return nil
        }

        if let url = URL(string: trimmed),
           let scheme = url.scheme?.lowercased(),
           scheme == "ironmesh",
           let components = URLComponents(url: url, resolvingAgainstBaseURL: false)
        {
            let preferredQueryNames = ["bootstrap", "payload", "connection"]
            for name in preferredQueryNames {
                if let value = components.queryItems?
                    .first(where: { $0.name.caseInsensitiveCompare(name) == .orderedSame })?.value,
                   let decoded = normalizedScannedPayload(value)
                {
                    return decoded
                }
            }
        }

        return normalizedScannedPayload(trimmed)
    }

    public static func looksLikeBootstrap(_ value: String) -> Bool {
        value.trimmingCharacters(in: .whitespacesAndNewlines).hasPrefix("{")
    }

    public static func looksLikeBootstrapClaim(_ value: String) -> Bool {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let data = trimmed.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let values = object as? [String: Any]
        else {
            return false
        }

        if values["kind"] as? String == "client_bootstrap_claim" {
            return true
        }

        // The compact claim form deliberately has only short field names. This is only a
        // recovery guard for stale native state; Rust remains the authoritative parser.
        return ["v", "k", "c", "n", "r", "t"].allSatisfy { values[$0] != nil }
    }

    private static func normalizedScannedPayload(_ value: String) -> String? {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return nil
        }

        if trimmed.hasPrefix("{") {
            return trimmed
        }

        if let decoded = trimmed.removingPercentEncoding,
           decoded != trimmed,
           decoded.hasPrefix("{")
        {
            return decoded
        }

        return nil
    }
}
