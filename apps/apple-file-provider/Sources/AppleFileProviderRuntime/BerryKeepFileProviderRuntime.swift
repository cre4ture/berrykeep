import AppleCore
@preconcurrency import FileProvider
import Foundation
import UniformTypeIdentifiers

struct BerryKeepBundleConfiguration {
    let domainIdentifier: String
    let domainDisplayName: String
    let bootstrapJSON: String
    let appGroupIdentifier: String?
    let keychainAccessGroup: String?
    let syncProfile: AppleSyncProfile?

    init(
        domainIdentifier: String,
        domainDisplayName: String = "BerryKeep",
        bootstrapJSON: String = "",
        appGroupIdentifier: String? = nil,
        keychainAccessGroup: String? = nil,
        syncProfile: AppleSyncProfile? = nil
    ) {
        self.domainIdentifier = domainIdentifier.nilIfBlank ?? "dev.berrykeep.default"
        self.domainDisplayName = domainDisplayName.nilIfBlank ?? "BerryKeep"
        self.bootstrapJSON = bootstrapJSON.nilIfBlank ?? ""
        self.appGroupIdentifier = appGroupIdentifier.nilIfBlank
        self.keychainAccessGroup = keychainAccessGroup.nilIfBlank
        self.syncProfile = syncProfile
    }

    init(bundle: Bundle = .main) {
        let info = bundle.infoDictionary ?? [:]
        domainIdentifier = (info["BerryKeepDomainIdentifier"] as? String)?.nilIfBlank ?? "dev.berrykeep.default"
        domainDisplayName = (info["BerryKeepDomainDisplayName"] as? String)?.nilIfBlank ?? "BerryKeep"
        bootstrapJSON = ""
        appGroupIdentifier = (info["BerryKeepAppGroupIdentifier"] as? String)?.nilIfBlank
        keychainAccessGroup = (info["BerryKeepKeychainAccessGroup"] as? String)?.nilIfBlank
        syncProfile = nil
    }

    init(bundle: Bundle, domain: NSFileProviderDomain, syncProfile: AppleSyncProfile?) {
        let bundled = Self(bundle: bundle)
        domainIdentifier = domain.identifier.rawValue
        domainDisplayName = syncProfile?.displayName ?? domain.displayName
        bootstrapJSON = bundled.bootstrapJSON
        appGroupIdentifier = bundled.appGroupIdentifier
        keychainAccessGroup = bundled.keychainAccessGroup
        self.syncProfile = syncProfile
    }

    var defaultConnectionConfiguration: AppleConnectionConfiguration {
        AppleConnectionConfiguration(connectionInput: bootstrapJSON)
    }

    var domain: NSFileProviderDomain {
        NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: domainIdentifier), displayName: domainDisplayName)
    }

    func makeSettingsStore() -> AppleConnectionSettingsStore {
        AppleConnectionSettingsStore(
            preferencesSuiteName: appGroupIdentifier,
            keychainAccessGroup: keychainAccessGroup
        )
    }

    func makeProfileStore() -> AppleSyncProfileStore {
        AppleSyncProfileStore(preferencesSuiteName: appGroupIdentifier)
    }
}
