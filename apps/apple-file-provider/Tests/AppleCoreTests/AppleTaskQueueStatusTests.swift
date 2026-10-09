import Foundation
import XCTest
@testable import AppleCore

final class AppleTaskQueueStatusTests: XCTestCase {
    func testDecodesClusterSnapshotWithoutLosingUnavailableNodes() throws {
        let data = Data(#"""
        {
          "generated_at_unix_ms": 1700000000123,
          "nodes": [{
            "node_id": "node-a",
            "queues": [{
              "id": "replication_repair",
              "label": "Replication repair",
              "pending": 3,
              "active": 1,
              "capacity": 1,
              "state": "backlogged",
              "detail": "startup state: Running"
            }]
          }],
          "unavailable_nodes": [{"node_id": "node-b", "error": "node is offline"}]
        }
        """#.utf8)

        let snapshot = try JSONDecoder().decode(AppleClusterTaskQueueSnapshot.self, from: data)

        XCTAssertEqual(snapshot.generatedAtUnixMs, 1_700_000_000_123)
        XCTAssertEqual(snapshot.nodes.first?.queues.first?.pending, 3)
        XCTAssertEqual(snapshot.nodes.first?.queues.first?.state, "backlogged")
        XCTAssertEqual(snapshot.unavailableNodes.first?.nodeID, "node-b")
    }
}
