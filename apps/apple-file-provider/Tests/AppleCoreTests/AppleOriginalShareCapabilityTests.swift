import Foundation
import XCTest
@testable import AppleCore

final class AppleOriginalShareCapabilityTests: XCTestCase {
    func testOriginalShareIdentifierRoundTripsWithoutExposingTheRemotePath() {
        let identifier = AppleFileProviderItemIdentifier.originalShare(
            token: "123e4567-e89b-12d3-a456-426614174000"
        )

        XCTAssertEqual(
            identifier.serialized,
            "share:original:123e4567-e89b-12d3-a456-426614174000"
        )
        XCTAssertEqual(
            AppleFileProviderItemIdentifier(serialized: identifier.serialized),
            identifier
        )
        XCTAssertNil(identifier.temporaryFilePath)
        XCTAssertNil(identifier.fileObjectID)
    }

    func testWebMessageRequiresExactlyOneImmutableSelector() throws {
        let snapshot = try AppleOriginalShareRequest.decodeWebMessage(
            webMessage(snapshotID: "snapshot-1", versionID: nil)
        )
        XCTAssertEqual(snapshot.remotePath, "gallery/cat.png")
        XCTAssertEqual(snapshot.snapshotID, "snapshot-1")
        XCTAssertNil(snapshot.versionID)
        XCTAssertEqual(snapshot.displayName, "cat.png")

        XCTAssertThrowsError(
            try AppleOriginalShareRequest.decodeWebMessage(
                webMessage(snapshotID: nil, versionID: nil)
            )
        )
        XCTAssertThrowsError(
            try AppleOriginalShareRequest.decodeWebMessage(
                webMessage(snapshotID: "snapshot-1", versionID: "version-1")
            )
        )
    }

    func testWebMessageSanitizesFilenameAndMimeType() throws {
        let request = try AppleOriginalShareRequest.decodeWebMessage(
            webMessage(
                snapshotID: nil,
                versionID: "version-1",
                fileName: "../bad/name?.jpg",
                mimeType: "not a mime"
            )
        )

        XCTAssertEqual(request.displayName, "name_.jpg")
        XCTAssertEqual(request.mimeType, "application/octet-stream")
        XCTAssertEqual(request.sizeBytes, 3_145_728)
    }

    func testCapabilityStorePersistsAndExpiresCapabilities() throws {
        let directory = temporaryDirectory()
        var now = Date(timeIntervalSince1970: 1_000)
        let store = AppleOriginalShareCapabilityStore(
            directoryURL: directory,
            clock: { now },
            tokenFactory: { "123e4567-e89b-12d3-a456-426614174000" }
        )
        let request = try AppleOriginalShareRequest.decodeWebMessage(
            webMessage(snapshotID: "snapshot-1", versionID: nil)
        )

        let capability = try store.create(request)
        XCTAssertEqual(try store.resolve(token: capability.token), capability)
        XCTAssertEqual(capability.selectorRevision, "snapshot:snapshot-1")

        now = capability.expiresAt
        XCTAssertThrowsError(try store.resolve(token: capability.token)) { error in
            XCTAssertEqual(error as? AppleOriginalShareError, .capabilityExpired)
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.path + "/\(capability.token).json"))
    }

    func testCapabilityStoreRejectsPathLikeTokens() throws {
        let store = AppleOriginalShareCapabilityStore(directoryURL: temporaryDirectory())

        XCTAssertThrowsError(try store.resolve(token: "../connection-state")) { error in
            XCTAssertEqual(error as? AppleOriginalShareError, .invalidToken)
        }
    }

    private func webMessage(
        snapshotID: String?,
        versionID: String?,
        fileName: String = "cat.png",
        mimeType: String = "image/png"
    ) throws -> Data {
        var object: [String: Any] = [
            "action": "share-original",
            "requestId": "request-1",
            "key": "/gallery/cat.png/",
            "fileName": fileName,
            "mimeType": mimeType,
            "sizeBytes": 3_145_728,
        ]
        object["snapshotId"] = snapshotID ?? NSNull()
        object["versionId"] = versionID ?? NSNull()
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    private func temporaryDirectory() -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("AppleOriginalShareCapabilityTests-\(UUID().uuidString)", isDirectory: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: directory)
        }
        return directory
    }
}
