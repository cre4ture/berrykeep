import Foundation
import XCTest
@testable import AppleCore

final class AppleConnectionRouteSnapshotTests: XCTestCase {
    func testDecodesCompleteRouteSnapshotAndPreservesRanking() throws {
        let snapshot = try decodeSnapshot(from: """
        {
          "generated_at_unix_ms": 1700000000000,
          "active_index": 1,
          "ranked_indices": [1, 2, 0],
          "endpoints": [
            {
              "index": 0,
              "path_kind": "direct_https",
              "locator": "https://node.example",
              "bootstrap_rank": 0,
              "target_node_id": null,
              "active": false,
              "score": 45.5,
              "ewma_latency_ms": 31.25,
              "ewma_throughput_bytes_per_sec": 4096.5,
              "consecutive_failures": 0,
              "total_failures": 2,
              "total_successes": 12,
              "last_measurement_unix_ms": 1699999999000,
              "last_success_unix_ms": 1699999999000,
              "last_failure_unix_ms": 1699999900000,
              "circuit_open_until_unix_ms": null,
              "background_probe_in_flight": false,
              "last_background_probe_unix_ms": 1699999998000,
              "last_error": null
            },
            {
              "index": 1,
              "path_kind": "direct_quic",
              "hole_punching_mode": "direct",
              "locator": "node.example:4433",
              "bootstrap_rank": 1,
              "target_node_id": "018f7630-7b60-7000-8000-000000000001",
              "active": true,
              "score": 12.75,
              "ewma_latency_ms": 8.5,
              "ewma_throughput_bytes_per_sec": null,
              "consecutive_failures": 0,
              "total_failures": 1,
              "total_successes": 30,
              "last_measurement_unix_ms": 1699999999500,
              "last_success_unix_ms": 1699999999500,
              "last_failure_unix_ms": null,
              "circuit_open_until_unix_ms": null,
              "background_probe_in_flight": true,
              "last_background_probe_unix_ms": 1699999999500,
              "last_error": null
            },
            {
              "index": 2,
              "path_kind": "relay_tunnel",
              "locator": "relay.example",
              "bootstrap_rank": 2,
              "target_node_id": null,
              "active": false,
              "score": 99.0,
              "ewma_latency_ms": null,
              "ewma_throughput_bytes_per_sec": null,
              "consecutive_failures": 3,
              "total_failures": 7,
              "total_successes": 4,
              "last_measurement_unix_ms": 1699999900000,
              "last_success_unix_ms": 1699999800000,
              "last_failure_unix_ms": 1699999900000,
              "circuit_open_until_unix_ms": 1700000060000,
              "background_probe_in_flight": false,
              "last_background_probe_unix_ms": null,
              "last_error": "relay unavailable"
            }
          ]
        }
        """)

        XCTAssertEqual(snapshot.activeEndpoint?.pathKind, .directQUIC)
        XCTAssertEqual(snapshot.rankedEndpoints.map(\.index), [1, 2, 0])
        XCTAssertEqual(snapshot.directEndpointCount, 2)
        XCTAssertEqual(snapshot.relayEndpointCount, 1)

        let quic = try XCTUnwrap(snapshot.activeEndpoint)
        XCTAssertEqual(quic.ewmaLatencyMs, 8.5)
        XCTAssertEqual(quic.totalSuccesses, 30)
        XCTAssertTrue(quic.backgroundProbeInFlight)
        XCTAssertTrue(quic.isDirectQuicHolePunched)
        XCTAssertFalse(quic.usesRelayPath)
        XCTAssertEqual(quic.connectionDisplayName, "Direct via NAT (QUIC)")
        XCTAssertEqual(quic.compactConnectionDisplayName, "QUIC NAT · 018f7630…0001")
        XCTAssertEqual(
            quic.connectionExplanation,
            "Rendezvous established this connection; data travels directly between this device and the cluster."
        )

        let relay = try XCTUnwrap(snapshot.endpoints.first(where: { $0.pathKind == .relayTunnel }))
        XCTAssertTrue(relay.isCoolingDown(atUnixMs: snapshot.generatedAtUnixMs))
        XCTAssertEqual(relay.lastError, "relay unavailable")
        XCTAssertEqual(relay.compactConnectionDisplayName, "Relay · relay.example")
    }

    func testCompactsRelayLocatorAfterIdentityAndScheme() {
        let endpoint = AppleConnectionRouteEndpoint(
            index: 0,
            pathKind: .relayTunnel,
            holePunchingMode: nil,
            locator: "node-a@https://creax.de:44043/path",
            bootstrapRank: 0,
            targetNodeId: "7314c3bb-2e1d-4508-a4d1-d274d985f059",
            active: true,
            score: 1,
            ewmaLatencyMs: nil,
            ewmaThroughputBytesPerSec: nil,
            consecutiveFailures: 0,
            totalFailures: 0,
            totalSuccesses: 1,
            lastMeasurementUnixMs: nil,
            lastSuccessUnixMs: nil,
            lastFailureUnixMs: nil,
            circuitOpenUntilUnixMs: nil,
            backgroundProbeInFlight: false,
            lastBackgroundProbeUnixMs: nil,
            lastError: nil
        )

        XCTAssertEqual(endpoint.compactConnectionDisplayName, "Relay · creax.de:44043")
    }

    func testUnknownPathKindRemainsDecodable() throws {
        let snapshot = try decodeSnapshot(from: """
        {
          "generated_at_unix_ms": 1,
          "active_index": null,
          "ranked_indices": [4],
          "endpoints": [{
            "index": 4,
            "path_kind": "future_mesh",
            "locator": "future.example",
            "bootstrap_rank": 0,
            "target_node_id": null,
            "active": false,
            "score": 1.0,
            "ewma_latency_ms": null,
            "ewma_throughput_bytes_per_sec": null,
            "consecutive_failures": 0,
            "total_failures": 0,
            "total_successes": 0,
            "last_measurement_unix_ms": null,
            "last_success_unix_ms": null,
            "last_failure_unix_ms": null,
            "circuit_open_until_unix_ms": null,
            "background_probe_in_flight": false,
            "last_background_probe_unix_ms": null,
            "last_error": null
          }]
        }
        """)

        XCTAssertEqual(snapshot.endpoints.first?.pathKind, .unknown("future_mesh"))
        XCTAssertEqual(snapshot.endpoints.first?.pathKind.displayName, "Future Mesh")
    }

    func testTreatsRelayAssistedQuicAsARelayPath() {
        let endpoint = AppleConnectionRouteEndpoint(
            index: 0,
            pathKind: .directQUIC,
            holePunchingMode: "relay",
            locator: "iroh://node.example",
            bootstrapRank: 0,
            targetNodeId: nil,
            active: true,
            score: 1,
            ewmaLatencyMs: nil,
            ewmaThroughputBytesPerSec: nil,
            consecutiveFailures: 0,
            totalFailures: 0,
            totalSuccesses: 1,
            lastMeasurementUnixMs: nil,
            lastSuccessUnixMs: nil,
            lastFailureUnixMs: nil,
            circuitOpenUntilUnixMs: nil,
            backgroundProbeInFlight: false,
            lastBackgroundProbeUnixMs: nil,
            lastError: nil
        )
        let snapshot = AppleConnectionRouteSnapshot(
            generatedAtUnixMs: 1,
            activeIndex: endpoint.index,
            rankedIndices: [endpoint.index],
            endpoints: [endpoint]
        )

        XCTAssertTrue(endpoint.usesRelayPath)
        XCTAssertFalse(endpoint.isDirectQuicHolePunched)
        XCTAssertEqual(endpoint.connectionDisplayName, "QUIC via Relay")
        XCTAssertEqual(snapshot.directEndpointCount, 0)
        XCTAssertEqual(snapshot.relayEndpointCount, 1)
    }

    private func decodeSnapshot(from json: String) throws -> AppleConnectionRouteSnapshot {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(AppleConnectionRouteSnapshot.self, from: Data(json.utf8))
    }
}
