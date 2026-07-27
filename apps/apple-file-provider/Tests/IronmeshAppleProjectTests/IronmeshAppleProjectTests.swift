import AppleCore
import AppleFileProviderShared
import XCTest

final class IronmeshAppleProjectTests: XCTestCase {
    func testSharedPackageTypesAreAvailableToTheXcodeProject() {
        let bootstrapJSON = #"{"version":1}"#
        let configuration = AppleConnectionConfiguration(connectionInput: bootstrapJSON)
        let item = AppleFileProviderItem.file(
            path: "docs/readme.txt",
            objectID: "demo-object-id"
        )

        XCTAssertEqual(configuration.normalizedConnectionInput, bootstrapJSON)
        XCTAssertEqual(item.identifier.serialized, "file:object:demo-object-id")
    }
}
