package io.ironmesh.android.macrobenchmark

import androidx.benchmark.macro.CompilationMode
import androidx.benchmark.macro.FrameTimingMetric
import androidx.benchmark.macro.junit4.MacrobenchmarkRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.filters.LargeTest
import androidx.test.uiautomator.By
import androidx.test.uiautomator.Direction
import androidx.test.uiautomator.Until
import kotlin.math.roundToInt
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@LargeTest
@RunWith(AndroidJUnit4::class)
class ForegroundPollingFrameStabilityBenchmark {
    @get:Rule
    val benchmarkRule = MacrobenchmarkRule()

    @Test
    fun foregroundPollingKeepsScrollFramesStable() = benchmarkRule.measureRepeated(
        packageName = TARGET_PACKAGE,
        metrics = listOf(FrameTimingMetric()),
        compilationMode = CompilationMode.Full(),
        iterations = ITERATIONS,
        startupMode = null,
        setupBlock = {
            device.executeShellCommand("pm clear $TARGET_PACKAGE")
            pressHome()
            startActivityAndWait()
            checkNotNull(
                device.wait(
                    Until.findObject(By.res(ONBOARDING_SCROLL_RESOURCE_ID)),
                    UI_WAIT_TIMEOUT_MILLIS,
                ),
            ) { "Onboarding scroll surface did not appear" }
        },
    ) {
        val scrollSurface = checkNotNull(
            device.findObject(By.res(ONBOARDING_SCROLL_RESOURCE_ID)),
        ) { "Onboarding scroll surface disappeared before measurement" }
        scrollSurface.setGestureMargin(device.displayWidth / GESTURE_MARGIN_DIVISOR)
        // Distance divided by this speed keeps each gesture near one second,
        // continuously rendering across the status monitors' polling cadence.
        val pixelsPerSecond =
            (scrollSurface.visibleBounds.height() * GESTURE_DISTANCE).roundToInt()
                .coerceAtLeast(MIN_GESTURE_SPEED_PIXELS_PER_SECOND)

        repeat(POLLING_WINDOWS_PER_ITERATION) { index ->
            val direction = if (index % 2 == 0) Direction.UP else Direction.DOWN
            scrollSurface.swipe(direction, GESTURE_DISTANCE, pixelsPerSecond)
        }
    }

    private companion object {
        const val TARGET_PACKAGE = "io.ironmesh.android"
        const val ONBOARDING_SCROLL_RESOURCE_ID = "onboarding_scroll"
        const val ITERATIONS = 5
        const val POLLING_WINDOWS_PER_ITERATION = 8
        const val UI_WAIT_TIMEOUT_MILLIS = 10_000L
        const val GESTURE_DISTANCE = 0.75f
        const val GESTURE_MARGIN_DIVISOR = 5
        const val MIN_GESTURE_SPEED_PIXELS_PER_SECOND = 200
    }
}
