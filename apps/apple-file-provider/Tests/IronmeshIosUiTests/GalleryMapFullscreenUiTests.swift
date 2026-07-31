import XCTest

final class GalleryMapFullscreenUiTests: XCTestCase {
    @MainActor
    func testFullscreenGalleryMapKeepsTheDirectEmbeddingVisible() {
        let app = launchApp(embeddedSurface: "gallery_map")
        let webView = app.webViews["ironmesh-hosted-web-ui"]
        XCTAssertTrue(webView.waitForExistence(timeout: 45), "The directly embedded Gallery Map should load")

        let fullscreenButton = element(in: webView, labelled: "Fullscreen map")
        XCTAssertTrue(fullscreenButton.waitForExistence(timeout: 45), "The direct Gallery Map should expose fullscreen")
        fullscreenButton.tap()

        assertFullscreenMapRemainsVisible(in: webView)
        let exitFullscreenButton = element(in: webView, labelled: "Exit fullscreen map")
        XCTAssertTrue(
            exitFullscreenButton.waitForExistence(timeout: 45),
            "The directly embedded Gallery Map should expose fullscreen controls"
        )
        assertFullscreenMapClusterChooserIsVisible(in: webView)
    }

    @MainActor
    func testFullscreenGalleryMapKeepsTheClientWebUIVisible() {
        let app = launchApp(webUIURL: "\(galleryRuntimeURL)?page=gallery&gallery_view=map")

        let webView = app.webViews["ironmesh-hosted-web-ui"]
        XCTAssertTrue(webView.waitForExistence(timeout: 45), "The embedded Client UI should load")

        let fullscreenButton = element(in: webView, labelled: "Fullscreen map")
        guard fullscreenButton.waitForExistence(timeout: 45) else {
            XCTFail("The Gallery map should load")
            return
        }
        fullscreenButton.tap()

        assertFullscreenMapRemainsVisible(in: webView)
        assertFullscreenMapClusterChooserIsVisible(in: webView)
    }

    @MainActor
    private func launchApp(
        embeddedSurface: String? = nil,
        webUIURL: String? = nil
    ) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchEnvironment["IRONMESH_UI_TEST_WEB_UI_URL"] = webUIURL ?? galleryRuntimeURL
        if let embeddedSurface {
            app.launchEnvironment["IRONMESH_UI_TEST_EMBEDDED_SURFACE"] = embeddedSurface
        }
        app.launch()
        return app
    }

    @MainActor
    private func assertFullscreenMapRemainsVisible(in webView: XCUIElement) {
        let fullscreenMap = largestElement(in: webView, labelled: "Geotagged gallery map")
        XCTAssertTrue(fullscreenMap.waitForExistence(timeout: 45), "The fullscreen map should remain visible")
    }

    @MainActor
    private func assertFullscreenMapClusterChooserIsVisible(in webView: XCUIElement) {
        let cluster = element(in: webView, labelled: "Open map cluster with 2 items")
        XCTAssertTrue(cluster.waitForExistence(timeout: 45), "The fullscreen map should expose the clustered image bubble")
        cluster.tap()

        let chooser = element(in: webView, labelled: "2 items in map cluster")
        XCTAssertTrue(chooser.waitForExistence(timeout: 45), "Selecting a map bubble should open its image chooser")
        XCTAssertGreaterThan(chooser.frame.height, 0, "The image chooser must have a visible height")
        XCTAssertTrue(
            element(in: webView, labelled: "gallery/runtime-map-a.png").waitForExistence(timeout: 10),
            "The first clustered image should be selectable"
        )
        XCTAssertTrue(
            element(in: webView, labelled: "gallery/runtime-map-b.png").waitForExistence(timeout: 10),
            "The second clustered image should be selectable"
        )
    }

    private var galleryRuntimeURL: String {
        ProcessInfo.processInfo.environment["IRONMESH_GALLERY_RUNTIME_URL"]
            ?? "http://127.0.0.1:18081/"
    }

    @MainActor
    private func element(in webView: XCUIElement, labelled label: String) -> XCUIElement {
        elements(in: webView, labelled: label).firstMatch
    }

    @MainActor
    private func largestElement(in webView: XCUIElement, labelled label: String) -> XCUIElement {
        let matches = elements(in: webView, labelled: label).allElementsBoundByIndex
        return matches.max(by: { $0.frame.height < $1.frame.height }) ?? element(in: webView, labelled: label)
    }

    @MainActor
    private func elements(in webView: XCUIElement, labelled label: String) -> XCUIElementQuery {
        webView
            .descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", label))
    }
}
