package io.berrykeep.android.ui

/**
 * The embedded Web UI is process-global in native code, so every ViewModel instance must
 * coordinate through the same lifecycle gate.
 */
internal object ProcessWebUiNativeLifecycle {
    val coordinator = NativeLifecycleOperationCoordinator()
}
