package io.ironmesh.android.data

object RustClientTestBridge {
    init {
        System.loadLibrary("android_app")
    }

    @JvmStatic
    external fun getAndroidSystemDnsServerCount(): Int

    @JvmStatic
    external fun startRendezvousRenewalScenario(): String

    @JvmStatic
    external fun startFolderSyncOutageScenario(): String

    @JvmStatic
    external fun setFolderSyncOutageScenarioAvailable(available: Boolean)

    @JvmStatic
    external fun getFolderSyncOutageDirectConnectionAttemptCount(): Int

    @JvmStatic
    external fun getFolderSyncOutageRendezvousContactAttemptCount(): Int

    @JvmStatic
    external fun getCapturedRequestPaths(): String

    @JvmStatic
    external fun getPairedSessionCount(): Int

    @JvmStatic
    external fun stopRendezvousRenewalScenario()
}
