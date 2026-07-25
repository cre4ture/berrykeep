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
        }
    }

    @Test
    fun standardClientWebUiKeepsTheMapVisibleAfterFullscreen() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val session = testSession("?page=gallery&gallery_view=map")
        EmbeddedWebUiSessionRegistry.activate(session)

        ActivityScenario.launch<WebUiActivity>(WebUiActivity.intent(context, session)).use { scenario ->
            assertFullscreenMapRemainsVisible(scenario)
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

        clickButton(webView, "Fullscreen map")
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

        assertEquals("Fullscreen must not finish the Web UI activity", Lifecycle.State.RESUMED, scenario.state)
        assertTrue("The embedded WebView must remain visible", webView.visibility == View.VISIBLE)
    }

    private fun hasButton(label: String): String =
        """
        (() => [...document.querySelectorAll('button')]
          .some((button) => button.textContent?.trim() === '$label'))()
        """.trimIndent()

    private fun clickButton(webView: WebView, label: String) {
        val clicked = evaluateBoolean(
            webView,
            """
            (() => {
              const button = [...document.querySelectorAll('button')]
                .find((candidate) => candidate.textContent?.trim() === '$label');
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
