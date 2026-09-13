import AppleCore
@preconcurrency import FileProvider
import Foundation
import UniformTypeIdentifiers

final class BerryKeepFileProviderService: @unchecked Sendable {
    let configuration: BerryKeepBundleConfiguration

    let bridge: AppleCFacadeBridge
    let ffi: AppleManualCBridgeFFI
    let cache: BerryKeepIdentifierPathCache
    let settingsStore: AppleConnectionSettingsStore
    let profileStore: AppleSyncProfileStore
    let pathMapper: AppleProfilePathMapper
    let environment: any BerryKeepSyncEnvironmentProviding
    let changeJournal: AppleRemoteChangeJournalStore
    let lock = NSLock()
    var connected = false
    var connectedConfiguration: AppleConnectionConfiguration?

    init(
        configuration: BerryKeepBundleConfiguration = BerryKeepBundleConfiguration(),
        ffi: AppleManualCBridgeFFI = BerryKeepRustFFIAdapter(),
        settingsStore: AppleConnectionSettingsStore? = nil,
        profileStore: AppleSyncProfileStore? = nil,
        environment: any BerryKeepSyncEnvironmentProviding = BerryKeepLiveSyncEnvironment.shared,
        changeJournal: AppleRemoteChangeJournalStore? = nil
    ) {
        self.configuration = configuration
        self.ffi = ffi
        bridge = AppleCFacadeBridge(ffi: ffi)
        cache = BerryKeepIdentifierPathCache(domainIdentifier: configuration.domainIdentifier)
        self.settingsStore = settingsStore ?? configuration.makeSettingsStore()
        self.profileStore = profileStore ?? configuration.makeProfileStore()
        pathMapper = AppleProfilePathMapper(
            remotePrefix: configuration.syncProfile?.remotePrefix ?? ""
        )
        self.environment = environment
        self.changeJournal = changeJournal ?? AppleRemoteChangeJournalStore(
            fileURL: berrykeepChangeJournalURL(domainIdentifier: configuration.domainIdentifier)
        )
    }
}
