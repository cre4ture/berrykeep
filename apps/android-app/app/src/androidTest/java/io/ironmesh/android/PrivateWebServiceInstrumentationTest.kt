package io.ironmesh.android

import android.app.Activity
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.Closeable
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.charset.StandardCharsets
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

/**
 * Exercises the browser-visible gateway sequence that previously stopped at a
 * blank WebView: an issued local origin redeems its launch token, receives a cookie,
 * follows the landing redirect, and renders the private service.
 */
@RunWith(AndroidJUnit4::class)
class PrivateWebServiceInstrumentationTest {
    @Test
    fun inAppPrivateServiceRedeemsTheLaunchAndRendersTheAuthenticatedPage() {
        LocalPrivateServiceServer().use { server ->
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val launchUrl = "http://home-nas-test.localhost:${server.port}/_ironmesh/open?token=test-token"

            ActivityScenario.launch<PrivateWebServiceActivity>(
                PrivateWebServiceActivity.intent(context, launchUrl),
            ).use { scenario ->
                val webView = waitForWebView(scenario)
                waitForCondition(webView, "the authenticated NAS page to render") {
                    "document.body?.innerText.includes('NAS ready') === true"
                }
                assertEquals(Lifecycle.State.RESUMED, scenario.state)

                scenario.recreate()
                val restoredWebView = waitForWebView(scenario)
                waitForCondition(restoredWebView, "the authenticated NAS page after recreation") {
                    "document.body?.innerText.includes('NAS ready') === true"
                }
                assertEquals(Lifecycle.State.RESUMED, scenario.state)
            }

            assertEquals(
                "Activity recreation must reuse the gateway session instead of redeeming the launch token again",
                1,
                server.openRequestCount(),
            )
            assertTrue(
                "Expected the redirected private-service request to include the gateway session cookie",
                server.sawAuthenticatedLanding(),
            )
        }
    }

    private fun <T : Activity> waitForWebView(scenario: ActivityScenario<T>): WebView {
        var webView: WebView? = null
        waitUntil("private-service WebView creation") {
            scenario.onActivity { activity ->
                webView = activity.window.decorView.findWebView()
            }
            webView != null
        }
        return requireNotNull(webView)
    }

    private fun waitForCondition(webView: WebView, description: String, script: () -> String) {
        waitUntil(description) { evaluateJavaScript(webView, script()) == "true" }
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
            "Timed out evaluating JavaScript in the private-service WebView",
            latch.await(JAVASCRIPT_TIMEOUT_SECONDS, TimeUnit.SECONDS),
        )
        assertNotNull("The private-service WebView returned no JavaScript result", result)
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

    private class LocalPrivateServiceServer : Closeable {
        private val serverSocket = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
        private val requests = Collections.synchronizedList(mutableListOf<RecordedRequest>())
        val port: Int = serverSocket.localPort
        private val worker = thread(name = "private-service-instrumentation-server", isDaemon = true) {
            while (!serverSocket.isClosed) {
                try {
                    serve(serverSocket.accept())
                } catch (_: java.io.IOException) {
                    if (!serverSocket.isClosed) {
                        throw AssertionError("Private-service test server failed while accepting a request")
                    }
                }
            }
        }

        fun openRequestCount(): Int = requests.count { it.path == "/_ironmesh/open" }

        fun sawAuthenticatedLanding(): Boolean = requests.any { request ->
            request.path == "/" && request.cookie.contains(SESSION_COOKIE)
        }

        override fun close() {
            serverSocket.close()
            worker.join(TimeUnit.SECONDS.toMillis(5))
        }

        private fun serve(socket: Socket) {
            socket.use { client ->
                val input = client.getInputStream().bufferedReader(StandardCharsets.ISO_8859_1)
                val requestLine = input.readLine() ?: return
                val headers = linkedMapOf<String, String>()
                while (true) {
                    val header = input.readLine()?.takeIf(String::isNotEmpty) ?: break
                    val separator = header.indexOf(':')
                    if (separator > 0) {
                        headers[header.substring(0, separator).lowercase()] = header.substring(separator + 1).trim()
                    }
                }
                val target = requestLine.split(' ', limit = 3).getOrElse(1) { "/" }
                val path = target.substringBefore('?')
                val cookie = headers["cookie"].orEmpty()
                requests += RecordedRequest(path = path, cookie = cookie)

                val response = when (path) {
                    "/_ironmesh/open" -> TestResponse(
                        body = "<!doctype html><meta http-equiv=\"refresh\" content=\"0;url=/\"><p>Opening private service</p>",
                        headers = mapOf(
                            "Set-Cookie" to "$SESSION_COOKIE=test-session; HttpOnly; SameSite=Strict; Path=/",
                        ),
                    )

                    "/" -> {
                        if (cookie.contains("$SESSION_COOKIE=test-session")) {
                            TestResponse(body = "<!doctype html><h1>NAS ready</h1>")
                        } else {
                            TestResponse(status = "401 Unauthorized", body = "missing private-service session")
                        }
                    }

                    else -> TestResponse(status = "404 Not Found", body = "not found")
                }
                client.getOutputStream().bufferedWriter(StandardCharsets.UTF_8).use { output ->
                    output.write("HTTP/1.1 ${response.status}\r\n")
                    output.write("Content-Type: text/html; charset=utf-8\r\n")
                    output.write("Content-Length: ${response.body.toByteArray(StandardCharsets.UTF_8).size}\r\n")
                    response.headers.forEach { (name, value) -> output.write("$name: $value\r\n") }
                    output.write("Connection: close\r\n\r\n")
                    output.write(response.body)
                    output.flush()
                }
            }
        }
    }

    private data class RecordedRequest(
        val path: String,
        val cookie: String,
    )

    private data class TestResponse(
        val status: String = "200 OK",
        val body: String,
        val headers: Map<String, String> = emptyMap(),
    )

    private companion object {
        const val SESSION_COOKIE = "ironmesh_service_gateway_session"
        const val WAIT_TIMEOUT_SECONDS = 30L
        const val JAVASCRIPT_TIMEOUT_SECONDS = 10L
        const val POLL_INTERVAL_MILLIS = 250L
    }
}
