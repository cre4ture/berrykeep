package io.berrykeep.servernode.android

import android.content.Context
import java.io.File

object RustServerNodeBridge {
    const val DEFAULT_BIND_HOST = "0.0.0.0"
    const val DEFAULT_BIND_PORT = 38443
    private const val LOCAL_UI_HOST = "127.0.0.1"

    init {
        System.loadLibrary("android_server_node_app")
    }

    @JvmStatic
    external fun startNode(dataDirPath: String, bindHost: String, bindPort: Int)

    @JvmStatic
    external fun stopNode()

    @JvmStatic
    private external fun statusJson(): String

    fun ensureStarted(context: Context) {
        val dataDir = dataDirectory(context)
        startNode(dataDir.absolutePath, DEFAULT_BIND_HOST, DEFAULT_BIND_PORT)
    }

    private fun dataDirectory(context: Context): File {
        val canonicalDirectory = File(context.noBackupFilesDir, "berrykeep-server-node")
        if (canonicalDirectory.exists()) return canonicalDirectory

        val legacyDirectory = File(context.noBackupFilesDir, "ironmesh-server-node")
        if (legacyDirectory.exists() && !legacyDirectory.renameTo(canonicalDirectory)) {
            return legacyDirectory
        }

        canonicalDirectory.mkdirs()
        return canonicalDirectory
    }

    fun stop() {
        stopNode()
    }

    fun localUiUrl(port: Int = DEFAULT_BIND_PORT): String = "https://$LOCAL_UI_HOST:$port"

    fun status(): ServerNodeStatus = ServerNodeStatus.fromJson(statusJson())
}
