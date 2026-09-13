import AppleCore
@preconcurrency import FileProvider
import Foundation
import UniformTypeIdentifiers

open class BerryKeepFileProviderExtensionHost: NSObject, NSFileProviderReplicatedExtension, @unchecked Sendable {
    public let domain: NSFileProviderDomain
    let service: BerryKeepFileProviderService
    let workingSetSignals: BerryKeepWorkingSetSignalCoordinator

    public required init(domain: NSFileProviderDomain) {
        self.domain = domain
        let bundle = Bundle(for: Self.self)
        let bundledConfiguration = BerryKeepBundleConfiguration(bundle: bundle)
        let profile = try? bundledConfiguration.makeProfileStore().profile(
            domainIdentifier: domain.identifier.rawValue
        )
        let configuration = BerryKeepBundleConfiguration(
            bundle: bundle,
            domain: domain,
            syncProfile: profile
        )
        service = BerryKeepFileProviderService(configuration: configuration)
        workingSetSignals = BerryKeepWorkingSetSignalCoordinator(configuration: configuration)
        super.init()
        workingSetSignals.start()
    }

    deinit {
        workingSetSignals.invalidate()
    }

    public func invalidate() {
        workingSetSignals.invalidate()
    }
}

#if os(macOS)
extension BerryKeepFileProviderExtensionHost: NSFileProviderPartialContentFetching {}
#endif
