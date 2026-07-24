import XCTest

final class GalleryMapFullscreenUiTests: XCTestCase {
    @MainActor
    func testFullscreenGalleryMapKeepsTheClientWebUIVisible() {
        let app = XCUIApplication()
        app.launchEnvironment["IRONMESH_UI_TEST_WEB_UI_URL"] = galleryRuntimeURL
        app.launch()

        let webView = app.webViews["ironmesh-hosted-web-ui"]
        XCTAssertTrue(webView.waitForExistence(timeout: 45), "The embedded Client UI should load")

        let navigationMenu = webView.buttons["Toggle navigation menu"]
        XCTAssertTrue(navigationMenu.waitForExistence(timeout: 45), "The Client UI navigation should load")
        navigationMenu.tap()

        let galleryNavigationItem = webView.staticTexts["Gallery"]
        XCTAssertTrue(galleryNavigationItem.waitForExistence(timeout: 45), "The Gallery navigation item should load")
        galleryNavigationItem.tap()

        let mapButton = element(in: webView, labelled: "Map")
        guard mapButton.waitForExistence(timeout: 45) else {
            XCTFail("The Gallery map button should load")
            return
        }
        mapButton.tap()

        let fullscreenButton = element(in: webView, labelled: "Fullscreen map")
        guard fullscreenButton.waitForExistence(timeout: 45) else {
            XCTFail("The Gallery map should load")
            return
        }
        fullscreenButton.tap()

        let fullscreenMap = element(in: webView, labelled: "Geotagged gallery map")
        XCTAssertTrue(fullscreenMap.waitForExistence(timeout: 45), "The fullscreen map should remain visible")
        XCTAssertGreaterThan(
            fullscreenMap.frame.height,
            webView.frame.height * 0.9,
            "The embedded Client UI should render the map at fullscreen height"
        )
    }

    private var galleryRuntimeURL: String {
        ProcessInfo.processInfo.environment["IRONMESH_GALLERY_RUNTIME_URL"]
            ?? "http://127.0.0.1:18081/"
    }

    @MainActor
    private func element(in webView: XCUIElement, labelled label: String) -> XCUIElement {
        webView
            .descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", label))
            .firstMatch
    }
}
