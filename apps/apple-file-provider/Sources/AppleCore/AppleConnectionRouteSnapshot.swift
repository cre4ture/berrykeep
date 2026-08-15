import Foundation

public enum AppleConnectionRoutePathKind: Hashable, Sendable, Codable {
    case directHTTPS
    case directQUIC
    case relayTunnel
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer().decode(String.self)
        switch value {
        case "direct_https":
            self = .directHTTPS
        case "direct_quic":
            self = .directQUIC
        case "relay_tunnel":
            self = .relayTunnel
        default:
            self = .unknown(value)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }

    public var rawValue: String {
        switch self {
        case .directHTTPS:
            return "direct_https"
        case .directQUIC:
            return "direct_quic"
        case .relayTunnel:
            return "relay_tunnel"
        case .unknown(let value):
            return value
        }
    }

    public var displayName: String {
        switch self {
        case .directHTTPS:
            return "Direct HTTPS"
        case .directQUIC:
            return "Direct QUIC"
        case .relayTunnel:
            return "Relay tunnel"
        case .unknown(let value):
            return value.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    public var isDirect: Bool {
        self == .directHTTPS || self == .directQUIC
    }
}

public struct AppleConnectionRouteEndpoint: Codable, Equatable, Identifiable, Sendable {
    public var index: Int
    public var pathKind: AppleConnectionRoutePathKind
    public var holePunchingMode: String?
    public var locator: String
    public var bootstrapRank: Int
    public var targetNodeId: String?
    public var nodeConnectionPriority: Int? = nil
    public var irohRelayUrls: [String]? = nil
    public var lastSuccessfulIrohRelayUrl: String? = nil
    public var score: Double
    public var ewmaLatencyMs: Double?
    public var ewmaThroughputBytesPerSec: Double?
    public var consecutiveFailures: UInt32
    public var totalFailures: UInt64
    public var totalSuccesses: UInt64
    public var lastMeasurementUnixMs: UInt64?
    public var lastSuccessUnixMs: UInt64?
    public var lastUsedUnixMs: UInt64?
    public var lastFailureUnixMs: UInt64?
    public var circuitOpenUntilUnixMs: UInt64?
    public var backgroundProbeInFlight: Bool
    public var lastBackgroundProbeUnixMs: UInt64?
    public var lastError: String?

    public var id: Int { index }

    public var usesRelayPath: Bool {
        pathKind == .relayTunnel ||
            (pathKind == .directQUIC && holePunchingMode == "relay")
    }

    public var isDirectQuicHolePunched: Bool {
        pathKind == .directQUIC && holePunchingMode == "direct"
    }

    public var connectionDisplayName: String {
        switch pathKind {
        case .directQUIC where isDirectQuicHolePunched:
            return "Direct via NAT (QUIC)"
        case .directQUIC where usesRelayPath:
            return "QUIC via Relay"
        default:
            return pathKind.displayName
        }
    }

    public var compactConnectionDisplayName: String {
        let prefix: String
        switch pathKind {
        case .directHTTPS:
            prefix = "HTTPS"
        case .directQUIC where isDirectQuicHolePunched:
            prefix = "QUIC NAT"
        case .directQUIC where usesRelayPath:
            prefix = "QUIC relay"
        case .directQUIC:
            prefix = "QUIC"
        case .relayTunnel:
            prefix = "Relay"
        case .unknown:
            prefix = pathKind.displayName
        }

        let destination: String?
        if pathKind == .relayTunnel {
            destination = compactRouteLocator(locator)
                ?? targetNodeId.map(compactRouteIdentifier)
        } else {
            destination = targetNodeId.map(compactRouteIdentifier)
                ?? compactRouteLocator(locator)
        }
        return destination.map { "\(prefix) · \($0)" } ?? prefix
    }

    public var connectionExplanation: String? {
        if isDirectQuicHolePunched {
            return "Rendezvous established this connection; data travels directly between this device and the cluster."
        }
        if pathKind == .directQUIC && usesRelayPath {
            return "This QUIC session is currently carried through a relay."
        }
        return nil
    }

    public func isCoolingDown(atUnixMs timestamp: UInt64) -> Bool {
        guard let circuitOpenUntilUnixMs else {
            return false
        }
        return circuitOpenUntilUnixMs > timestamp
    }

    public func wasRecentlyUsed(
        atUnixMs timestamp: UInt64,
        withinMilliseconds window: UInt64 = 2_000
    ) -> Bool {
        guard let lastUsedUnixMs, lastUsedUnixMs <= timestamp else {
            return false
        }
        return timestamp - lastUsedUnixMs <= window
    }
}

private func compactRouteIdentifier(_ value: String) -> String {
    let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
    guard trimmed.count > 16 else {
        return trimmed
    }
    return "\(trimmed.prefix(8))…\(trimmed.suffix(4))"
}

private func compactRouteLocator(_ locator: String) -> String? {
    var value = locator.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !value.isEmpty else {
        return nil
    }
    if let atIndex = value.lastIndex(of: "@") {
        value = String(value[value.index(after: atIndex)...])
    }
    if let schemeRange = value.range(of: "://") {
        value = String(value[schemeRange.upperBound...])
    }
    if let slashIndex = value.firstIndex(of: "/") {
        value = String(value[..<slashIndex])
    }
    guard !value.isEmpty else {
        return nil
    }
    return compactRouteIdentifier(value)
}

public struct AppleConnectionRouteSnapshot: Codable, Equatable, Sendable {
    public var generatedAtUnixMs: UInt64
    public var rankedIndices: [Int]
    public var endpoints: [AppleConnectionRouteEndpoint]

    public var recentlyUsedEndpoints: [AppleConnectionRouteEndpoint] {
        endpoints
            .filter { $0.wasRecentlyUsed(atUnixMs: generatedAtUnixMs) }
            .sorted { ($0.lastUsedUnixMs ?? 0) > ($1.lastUsedUnixMs ?? 0) }
    }

    public var mostRecentlyUsedEndpoint: AppleConnectionRouteEndpoint? {
        endpoints
            .filter { $0.lastUsedUnixMs != nil }
            .max { ($0.lastUsedUnixMs ?? 0) < ($1.lastUsedUnixMs ?? 0) }
    }

    public var preferredEndpoint: AppleConnectionRouteEndpoint? {
        rankedEndpoints.first
    }

    public var displayEndpoint: AppleConnectionRouteEndpoint? {
        recentlyUsedEndpoints.first ?? preferredEndpoint
    }

    public var rankedEndpoints: [AppleConnectionRouteEndpoint] {
        let endpointsByIndex = endpoints.reduce(into: [Int: AppleConnectionRouteEndpoint]()) {
            if $0[$1.index] == nil {
                $0[$1.index] = $1
            }
        }
        var seen = Set<Int>()
        var result = rankedIndices.compactMap { index -> AppleConnectionRouteEndpoint? in
            guard seen.insert(index).inserted else {
                return nil
            }
            return endpointsByIndex[index]
        }
        let remaining = endpoints
            .filter { seen.insert($0.index).inserted }
            .sorted { $0.index < $1.index }
        result.append(contentsOf: remaining)
        return result
    }

    public var directEndpointCount: Int {
        endpoints.filter { $0.pathKind.isDirect && !$0.usesRelayPath }.count
    }

    public var relayEndpointCount: Int {
        endpoints.filter(\.usesRelayPath).count
    }
}
