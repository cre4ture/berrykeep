package io.berrykeep.android.work

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Process
import android.provider.DocumentsContract
import io.berrykeep.android.data.FolderSyncConfig

data class FolderSyncProfileEligibility(
    val profile: FolderSyncConfig,
    val allowed: Boolean,
    val reason: String,
    val folderAccessRequired: Boolean = false,
)

/** Shared by continuous sync, the recovery worker, and the UI. Never scans a folder. */
object FolderSyncEligibilityGate {
    fun hasFolderAccess(context: Context, profile: FolderSyncConfig): Boolean {
        val treeUriString = profile.localFolderTreeUri?.takeIf { it.isNotBlank() }
            ?: return true // Profiles without a SAF URI use the native filesystem backend.
        return runCatching {
            val treeUri = Uri.parse(treeUriString)
            val documentUri = DocumentsContract.buildDocumentUriUsingTree(
                treeUri, DocumentsContract.getTreeDocumentId(treeUri),
            )
            context.checkUriPermission(
                documentUri, Process.myPid(), Process.myUid(),
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
            ) == PackageManager.PERMISSION_GRANTED
        }.getOrDefault(false)
    }

    fun evaluateProfiles(context: Context, profiles: List<FolderSyncConfig>): List<FolderSyncProfileEligibility> =
        evaluateProfiles(profiles, FolderSyncNetworkGate.currentSnapshot(context)) { hasFolderAccess(context, it) }

    internal fun evaluateProfiles(
        profiles: List<FolderSyncConfig>,
        network: FolderSyncNetworkSnapshot,
        hasFolderAccess: (FolderSyncConfig) -> Boolean,
    ): List<FolderSyncProfileEligibility> = profiles.map { profile ->
        if (!hasFolderAccess(profile)) {
            FolderSyncProfileEligibility(
                profile, allowed = false, folderAccessRequired = true,
                reason = "Folder access required. Grant folder access to resume sync.",
            )
        } else {
            val networkDecision = FolderSyncNetworkGate.evaluate(profile.networkPolicy, network)
            FolderSyncProfileEligibility(profile, networkDecision.allowed, networkDecision.reason)
        }
    }
}
