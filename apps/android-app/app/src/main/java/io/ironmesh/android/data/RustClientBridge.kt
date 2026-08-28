package io.ironmesh.android.data

import android.content.Context
import java.io.InputStream
import java.io.OutputStream

object RustClientBridge {
    init {
        System.loadLibrary("android_app")
    }

    fun initialize(context: Context) {
        initializeAndroidContext(context.applicationContext)
    }

    @JvmStatic
    private external fun initializeAndroidContext(applicationContext: Context)

    @JvmStatic
    external fun enrollWithBootstrap(
        bootstrapJson: String,
        deviceId: String?,
        label: String?,
    ): String

    @JvmStatic
    external fun putObject(
        connectionInput: String,
        key: String,
        payload: ByteArray,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): Int

    @JvmStatic
    external fun getObject(
        connectionInput: String,
        key: String,
        snapshot: String?,
        version: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): ByteArray

    @JvmStatic
    external fun storeIndex(
        connectionInput: String,
        prefix: String?,
        depth: Int,
        snapshot: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): String

    @JvmStatic
    external fun storeIndexWithOptions(
        connectionInput: String,
        prefix: String?,
        depth: Int,
        snapshot: String?,
        view: String?,
        offset: Int,
        limit: Int,
        sort: String?,
        mediaFilter: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): String

    @JvmStatic
    external fun streamPutObject(
        connectionInput: String,
        key: String,
        input: InputStream,
        sizeBytes: Long,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): Int

    @JvmStatic
    external fun deleteObject(
        connectionInput: String,
        key: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): Int

    @JvmStatic
    external fun streamObjectTo(
        connectionInput: String,
        key: String,
        output: OutputStream,
        snapshot: String?,
        version: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun getObjectSize(
        connectionInput: String,
        key: String,
        snapshot: String?,
        version: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): Long

    @JvmStatic
    external fun readObjectRange(
        connectionInput: String,
        key: String,
        offset: Long,
        length: Int,
        snapshot: String?,
        version: String?,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): ByteArray

    @JvmStatic
    external fun streamRelativeUrlTo(
        connectionInput: String,
        relativeUrl: String,
        output: OutputStream,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun startWebUi(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): String

    @JvmStatic
    external fun stopWebUi()

    /** Clears the app's discardable cache files and stops a currently hosted local Web UI. */
    @JvmStatic
    external fun clearCachedData()

    @JvmStatic
    external fun configureTitleLatencyMonitor(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
        enabled: Boolean,
        periodSeconds: Long,
    ): String

    @JvmStatic
    external fun stopTitleLatencyMonitor()

    @JvmStatic
    external fun getTitleLatencyStatus(): String

    @JvmStatic
    external fun getDiagnosticLog(): String

    /**
     * Mirrors an already-sanitized embedded WebView diagnostic into the native retained log.
     * The native side applies its own length and line-break guard before storing it.
     */
    @JvmStatic
    external fun recordEmbeddedWebUiDiagnostic(message: String)

    @JvmStatic
    external fun getConnectionRouteSnapshot(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
        refresh: Boolean,
    ): String

    @JvmStatic
    external fun notifyNetworkChanged(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun notifyForegrounded(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun resetConnectionTimingMeasurement(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): String

    @JvmStatic
    external fun runConnectionTimingStoreIndexTest(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ): String

    @JvmStatic
    external fun runFolderSyncOnce(
        connectionInput: String,
        localFolder: String,
        localFolderTreeUri: String?,
        prefix: String?,
        depth: Int,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun startContinuousFolderSync(
        profileId: String,
        label: String,
        connectionInput: String,
        localFolder: String,
        localFolderTreeUri: String?,
        prefix: String?,
        depth: Int,
        serverCaPem: String?,
        clientIdentityJson: String?,
    )

    @JvmStatic
    external fun stopContinuousFolderSync(profileId: String)

    @JvmStatic
    external fun stopAllContinuousFolderSync()

    @JvmStatic
    external fun getContinuousFolderSyncStatus(): String

    @JvmStatic
    external fun getFolderSyncModificationHistory(
        connectionInput: String,
        localFolder: String,
        localFolderTreeUri: String?,
        prefix: String?,
        limit: Int,
        beforeId: Long,
        operation: String?,
    ): String

    @JvmStatic
    external fun hasContinuousFolderSyncActive(): Boolean
}
