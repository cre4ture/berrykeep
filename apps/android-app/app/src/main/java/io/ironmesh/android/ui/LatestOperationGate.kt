package io.ironmesh.android.ui

import java.util.concurrent.atomic.AtomicLong

/** Lets asynchronous callers discard work that no longer represents the latest intent. */
internal class LatestOperationGate {
    private val generation = AtomicLong(0)

    fun next(): Long = generation.incrementAndGet()

    fun isCurrent(candidate: Long): Boolean = generation.get() == candidate
}
