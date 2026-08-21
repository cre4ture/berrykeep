package io.ironmesh.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.data.EmbeddedWebUiSessionRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class GalleryMapFullscreenInstrumentationTest {
    @After
    fun clearEmbeddedWebUiSession() {
        EmbeddedWebUiSessionRegistry.clear()
    }

    @Test
    fun directGalleryMapEmbeddingKeepsTheMapVisibleAfterFullscreen() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val session = testSession()
        val intent = Intent(context, GalleryMapWebUiTestActivity::class.java)
            .putExtra(GalleryMapWebUiTestActivity.EXTRA_WEB_UI_URL, session.url)
            .putExtra(GalleryMapWebUiTestActivity.EXTRA_WEB_UI_AUTHORIZATION, session.authorization)

        ActivityScenario.launch<GalleryMapWebUiTestActivity>(intent).use { scenario ->
            assertFullscreenMapRemainsVisible(scenario)
            assertDirectGalleryMapUsesTheFullAppSurface(scenario)
            assertFullscreenExitRestoresTheNativeAppChrome(scenario)
            assertFullscreenMapRemainsVisible(scenario)
            assertFullscreenMapClusterChooserIsVisible(scenario)
            assertFullscreenMapLightboxIsVisible(scenario)
        }
    }

    @Test
    fun standardClientWebUiKeepsTheMapVisibleAfterFullscreen() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val session = testSession("?page=gallery&gallery_view=map")
        EmbeddedWebUiSessionRegistry.activate(session)

        ActivityScenario.launch<WebUiActivity>(WebUiActivity.intent(context, session)).use { scenario ->
            assertFullscreenMapRemainsVisible(scenario)
            assertFullscreenMapClusterChooserIsVisible(scenario)
            assertFullscreenMapLightboxIsVisible(scenario)
        }
    }

    private fun testSession(pathAndQuery: String = ""): EmbeddedWebUiSession =
        EmbeddedWebUiSession(
            url = "http://10.0.2.2:18081/$pathAndQuery",
            authorization = "gallery-map-simulator-test",
        )

    private fun <T : Activity> assertFullscreenMapRemainsVisible(scenario: ActivityScenario<T>) {
        val webView = waitForWebView(scenario)
        waitForCondition(webView, "Client UI should load the gallery map") {
            "document.readyState === 'complete' && Boolean(document.body)"
        }

        waitForCondition(webView, "Gallery map should load") {
            "document.body?.innerText.includes('Geo-tagged world map') === true"
        }
        waitForCondition(webView, "Gallery map should expose fullscreen") {
            hasButton("Fullscreen map")
        }
        waitForMapCluster(
            webView,
            "gallery map should expose a two-item cluster before fullscreen",
        )

        clickButton(webView, "Fullscreen map")
        try {
            waitForCondition(webView, "fullscreen gallery map should remain rendered") {
                """
                (() => {
                  const map = document.querySelector('[aria-label="Geotagged gallery map"]');
                  const mapRect = map?.getBoundingClientRect();
                  return Boolean(
                    map &&
                    mapRect &&
                    mapRect.width > 0 &&
                    mapRect.height > 0 &&
                    getComputedStyle(map).position === 'fixed'
                  );
                })()
                """.trimIndent()
            }
        } catch (error: AssertionError) {
            throw AssertionError(
                "${error.message}; fullscreen DOM state: ${fullscreenMapState(webView)}",
                error,
            )
        }

        assertEquals("Fullscreen must not finish the Web UI activity", Lifecycle.State.RESUMED, scenario.state)
        assertTrue("The embedded WebView must remain visible", webView.visibility == View.VISIBLE)
        waitForCondition(webView, "fullscreen gallery map should expose an exit control") {
            """
            (() => {
              const control = document.querySelector('[data-gallery-map-fullscreen-exit]');
              const controlRect = control?.getBoundingClientRect();
              return Boolean(
                control &&
                controlRect &&
                controlRect.width > 0 &&
                controlRect.height > 0 &&
                getComputedStyle(control).position === 'fixed' &&
                Number(getComputedStyle(control).zIndex) > 150
              );
            })()
            """.trimIndent()
        }
    }

    private fun assertDirectGalleryMapUsesTheFullAppSurface(
        scenario: ActivityScenario<GalleryMapWebUiTestActivity>,
    ) {
        val webView = waitForWebView(scenario)
        waitUntil("fullscreen map should fill the native app content surface") {
            scenario.onActivity { activity ->
                val appContent = activity.findViewById<View>(android.R.id.content)
                assertEquals(appContent.width, webView.width)
                assertEquals(appContent.height, webView.height)
            }
            true
        }
    }

    private fun assertFullscreenExitRestoresTheNativeAppChrome(
        scenario: ActivityScenario<GalleryMapWebUiTestActivity>,
    ) {
        val webView = waitForWebView(scenario)
        clickButton(webView, "Exit fullscreen map")
        waitForCondition(webView, "gallery map should leave fullscreen") {
            """
            (() => {
              const map = document.querySelector('[aria-label="Geotagged gallery map"]');
              return Boolean(map && getComputedStyle(map).position === 'relative');
            })()
            """.trimIndent()
        }
        waitUntil("native app chrome should constrain the direct gallery map again") {
            scenario.onActivity { activity ->
                val appContent = activity.findViewById<View>(android.R.id.content)
                assertTrue(
                    "Expected the native app chrome to reserve space after fullscreen exits",
                    webView.width < appContent.width || webView.height < appContent.height,
                )
            }
            true
        }
    }

    private fun <T : Activity> assertFullscreenMapClusterChooserIsVisible(scenario: ActivityScenario<T>) {
        val webView = waitForWebView(scenario)
        waitForMapCluster(webView, "fullscreen gallery map should retain a two-item cluster")

        clickButton(webView, "Open map cluster with 2 items")
        try {
            waitForCondition(webView, "fullscreen map cluster chooser should remain visible") {
                """
                (() => {
                  const dialog = document.querySelector('[data-gallery-map-cluster-dialog] [role="dialog"]');
                  const dialogRect = dialog?.getBoundingClientRect();
                  const choices = [...(dialog?.querySelectorAll('button') ?? [])]
                    .map((button) => button.textContent?.trim());
                  return Boolean(
                    dialog &&
                    dialogRect &&
                    dialogRect.width > 0 &&
                    dialogRect.height > 0 &&
                    choices.includes('gallery/runtime-map-a.png') &&
                    choices.includes('gallery/runtime-map-b.png')
                  );
                })()
                """.trimIndent()
            }
        } catch (error: AssertionError) {
            throw AssertionError(
                "${error.message}; cluster dialog DOM state: ${clusterDialogState(webView)}",
                error,
            )
        }
    }

    private fun <T : Activity> assertFullscreenMapLightboxIsVisible(scenario: ActivityScenario<T>) {
        val webView = waitForWebView(scenario)
        clickButton(webView, "gallery/runtime-map-a.png")
        try {
            waitForCondition(webView, "fullscreen map photo lightbox should remain visible") {
                """
                (() => {
                  const dialog = document.querySelector('[data-media-lightbox] [role="dialog"]');
                  const dialogRect = dialog?.getBoundingClientRect();
                  const image = dialog?.querySelector('img');
                  const imageRect = image?.getBoundingClientRect();
                  return Boolean(
                    dialog &&
                    dialogRect &&
                    dialogRect.width > 0 &&
                    dialogRect.height > 0 &&
                    image &&
                    imageRect &&
                    imageRect.width > 0 &&
                    imageRect.height > 0 &&
                    dialog.textContent?.includes('gallery/runtime-map-a.png') &&
                    dialog.textContent?.includes('image/png') &&
                    dialog.textContent?.includes('Captured')
                  );
                })()
                """.trimIndent()
            }
        } catch (error: AssertionError) {
            throw AssertionError(
                "${error.message}; media lightbox DOM state: ${mediaLightboxState(webView)}",
                error,
            )
        }
    }

    private fun fullscreenMapState(webView: WebView): String =
        try {
            evaluateJavaScript(
                webView,
                """
                (() => {
                  const map = document.querySelector('[aria-label="Geotagged gallery map"]');
                  const mapRect = map?.getBoundingClientRect();
                  return JSON.stringify({
                    href: window.location.href,
                    hasExitControl: [...document.querySelectorAll('button')]
                      .some((button) => button.textContent?.trim() === 'Exit fullscreen map'),
                    mapCount: document.querySelectorAll('[aria-label="Geotagged gallery map"]').length,
                    mapPosition: map ? getComputedStyle(map).position : null,
                    mapDisplay: map ? getComputedStyle(map).display : null,
                    mapWidth: mapRect?.width ?? null,
                    mapHeight: mapRect?.height ?? null,
                    mapParent: map?.parentElement?.tagName ?? null,
                    bodyOverflow: document.body.style.overflow
                  });
                })()
                """.trimIndent(),
            )
        } catch (diagnosticError: Throwable) {
            "unavailable (${diagnosticError.message})"
        }

    private fun clusterDialogState(webView: WebView): String =
        try {
            evaluateJavaScript(
                webView,
                """
                (() => {
                  const dialog = document.querySelector('[data-gallery-map-cluster-dialog] [role="dialog"]');
                  const dialogRect = dialog?.getBoundingClientRect();
                  return JSON.stringify({
                    dialogExists: Boolean(dialog),
                    dialogWidth: dialogRect?.width ?? null,
                    dialogHeight: dialogRect?.height ?? null,
                    dialogMaxHeight: dialog ? getComputedStyle(dialog).maxHeight : null,
                    dialogChoices: [...(dialog?.querySelectorAll('button') ?? [])]
                      .map((button) => button.textContent?.trim())
                  });
                })()
                """.trimIndent(),
            )
        } catch (diagnosticError: Throwable) {
            "unavailable (${diagnosticError.message})"
        }

    private fun mediaLightboxState(webView: WebView): String =
        try {
            evaluateJavaScript(
                webView,
                """
                (() => {
                  const dialog = document.querySelector('[data-media-lightbox] [role="dialog"]');
                  const dialogRect = dialog?.getBoundingClientRect();
                  const image = dialog?.querySelector('img');
                  const imageRect = image?.getBoundingClientRect();
                  return JSON.stringify({
                    dialogExists: Boolean(dialog),
                    dialogWidth: dialogRect?.width ?? null,
                    dialogHeight: dialogRect?.height ?? null,
                    imageExists: Boolean(image),
                    imageWidth: imageRect?.width ?? null,
                    imageHeight: imageRect?.height ?? null,
                    dialogText: dialog?.textContent?.trim() ?? null
                  });
                })()
                """.trimIndent(),
            )
        } catch (diagnosticError: Throwable) {
            "unavailable (${diagnosticError.message})"
        }

    private fun waitForMapCluster(webView: WebView, description: String) {
        try {
            waitForCondition(webView, description) {
                hasButton("Open map cluster with 2 items")
            }
        } catch (error: AssertionError) {
            throw AssertionError(
                "${error.message}; map cluster DOM state: ${mapClusterState(webView)}",
                error,
            )
        }
    }

    private fun mapClusterState(webView: WebView): String =
        try {
            evaluateJavaScript(
                webView,
                """
                (() => {
                  const map = document.querySelector('[aria-label="Geotagged gallery map"]');
                  const mapRect = map?.getBoundingClientRect();
                  return JSON.stringify({
                    mapWidth: mapRect?.width ?? null,
                    mapHeight: mapRect?.height ?? null,
                    visibleButtons: [...document.querySelectorAll('button')]
                      .filter((button) => button.getBoundingClientRect().width > 0)
                      .map((button) => button.textContent?.trim()),
                    hasBasemapLoader: document.body?.innerText.includes('Loading self-hosted basemap') ?? false,
                    hasBasemapError: document.body?.innerText.includes('Could not load the self-hosted basemap') ?? false,
                    canvasCount: map?.querySelectorAll('canvas').length ?? 0
                  });
                })()
                """.trimIndent(),
            )
        } catch (diagnosticError: Throwable) {
            "unavailable (${diagnosticError.message})"
        }

    private fun hasButton(label: String): String =
        """
        (() => [...document.querySelectorAll('button')]
          .some((button) =>
            button.textContent?.trim() === '$label' ||
            button.getAttribute('aria-label') === '$label'
          ))()
        """.trimIndent()

    private fun clickButton(webView: WebView, label: String) {
        val clicked = evaluateBoolean(
            webView,
            """
            (() => {
              const button = [...document.querySelectorAll('button')]
                .find((candidate) =>
                  candidate.textContent?.trim() === '$label' ||
                  candidate.getAttribute('aria-label') === '$label'
                );
              if (!button) return false;
              button.click();
              return true;
            })()
            """.trimIndent(),
        )
        assertTrue("Expected a '$label' button in the Client UI", clicked)
    }

    private fun <T : Activity> waitForWebView(scenario: ActivityScenario<T>): WebView {
        var webView: WebView? = null
        waitUntil("embedded WebView should be created") {
            scenario.onActivity { activity ->
                webView = activity.window.decorView.findWebView()
            }
            webView != null
        }
        return requireNotNull(webView)
    }

    private fun waitForCondition(webView: WebView, description: String, script: () -> String) {
        waitUntil(description) { evaluateBoolean(webView, script()) }
    }

    private fun waitUntil(description: String, condition: () -> Boolean) {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(WAIT_TIMEOUT_SECONDS)
        var lastFailure: Throwable? = null
        while (System.nanoTime() < deadline) {
            try {
                if (condition()) {
                    return
                }
            } catch (error: Throwable) {
                lastFailure = error
            }
            Thread.sleep(POLL_INTERVAL_MILLIS)
        }
        throw AssertionError("Timed out waiting for $description", lastFailure)
    }

    private fun evaluateBoolean(webView: WebView, script: String): Boolean {
        val result = evaluateJavaScript(webView, script)
        return result == "true"
    }

    private fun evaluateJavaScript(webView: WebView, script: String): String {
        val latch = CountDownLatch(1)
        var result: String? = null
        InstrumentationRegistry.getInstrumentation().runOnMainSync {
            webView.evaluateJavascript(script) { value ->
                result = value
                latch.countDown()
            }
        }
        assertTrue(
            "Timed out evaluating JavaScript in the embedded WebView",
            latch.await(JAVASCRIPT_TIMEOUT_SECONDS, TimeUnit.SECONDS),
        )
        assertNotNull("The embedded WebView returned no JavaScript result", result)
        return requireNotNull(result)
    }

    private fun View.findWebView(): WebView? {
        if (this is WebView) {
            return this
        }
        if (this !is ViewGroup) {
            return null
        }
        for (index in 0 until childCount) {
            val webView = getChildAt(index).findWebView()
            if (webView != null) {
                return webView
            }
        }
        return null
    }

    private companion object {
        const val WAIT_TIMEOUT_SECONDS = 45L
        const val JAVASCRIPT_TIMEOUT_SECONDS = 10L
        const val POLL_INTERVAL_MILLIS = 250L
    }
}
