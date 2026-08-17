import AppleCore
@preconcurrency import FileProvider
import Foundation
import UIKit
import UniformTypeIdentifiers

@MainActor
final class IronmeshOriginalShareCoordinator {
    private let configuration: IronmeshBundleConfiguration
    private let capabilityStore: AppleOriginalShareCapabilityStore
    private let domainCoordinator: AppleFileProviderDomainCoordinator

    init(
        configuration: IronmeshBundleConfiguration = IronmeshBundleConfiguration(bundle: .main),
        domainCoordinator: AppleFileProviderDomainCoordinator = AppleFileProviderDomainCoordinator()
    ) throws {
        guard let appGroupIdentifier = configuration.appGroupIdentifier else {
            throw IronmeshOriginalShareCoordinatorError.appGroupUnavailable
        }
        self.configuration = configuration
        capabilityStore = try AppleOriginalShareCapabilityStore(
            appGroupIdentifier: appGroupIdentifier
        )
        self.domainCoordinator = domainCoordinator
    }

    func presentShare(messageBody: Any, from presenter: UIViewController) async throws -> String {
        let messageData = try Self.messageData(messageBody)
        let request = try AppleOriginalShareRequest.decodeWebMessage(messageData)
        let capabilityStore = capabilityStore
        let capability = try await Task.detached(priority: .userInitiated) {
            try capabilityStore.create(request)
        }.value

        do {
            let fileURL = try await userVisibleURL(for: capability)
            try presentActivityController(
                capability: capability,
                fileURL: fileURL,
                from: presenter
            )
            return request.requestID
        } catch {
            capabilityStore.remove(token: capability.token)
            throw error
        }
    }

    private func userVisibleURL(for capability: AppleOriginalShareCapability) async throws -> URL {
        let registration = await domainCoordinator.register(
            identifier: configuration.domainIdentifier,
            displayName: configuration.domainDisplayName
        )
        guard registration.state.isRegistered else {
            throw IronmeshOriginalShareCoordinatorError.fileProviderUnavailable(
                registration.state.detail
            )
        }
        let domain = NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(rawValue: configuration.domainIdentifier),
            displayName: configuration.domainDisplayName
        )
        guard let manager = NSFileProviderManager(for: domain) else {
            throw IronmeshOriginalShareCoordinatorError.fileProviderUnavailable(
                "The iOS File Provider manager is unavailable."
            )
        }
        try await signalWorkingSet(manager)
        return try await manager.getUserVisibleURL(
            for: NSFileProviderItemIdentifier(
                rawValue: AppleFileProviderItemIdentifier.originalShare(
                    token: capability.token
                ).serialized
            )
        )
    }

    private func signalWorkingSet(_ manager: NSFileProviderManager) async throws {
        try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<Void, any Error>) in
            manager.signalEnumerator(for: .workingSet) { error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume(returning: ())
                }
            }
        }
    }

    private func presentActivityController(
        capability: AppleOriginalShareCapability,
        fileURL: URL,
        from presenter: UIViewController
    ) throws {
        guard presenter.presentedViewController == nil else {
            throw IronmeshOriginalShareCoordinatorError.presentationUnavailable
        }
        let contentType = UTType(mimeType: capability.mimeType)
            ?? UTType(filenameExtension: (capability.displayName as NSString).pathExtension)
            ?? .data
        let itemProvider = NSItemProvider()
        itemProvider.suggestedName = capability.displayName
        itemProvider.registerFileRepresentation(
            for: contentType,
            visibility: .all,
            openInPlace: true
        ) { completionHandler in
            completionHandler(fileURL, true, nil)
            return nil
        }

        let activityController = UIActivityViewController(
            activityItems: [itemProvider],
            applicationActivities: nil
        )
        if let popover = activityController.popoverPresentationController {
            popover.sourceView = presenter.view
            popover.sourceRect = CGRect(
                x: presenter.view.bounds.midX,
                y: presenter.view.bounds.midY,
                width: 1,
                height: 1
            )
            popover.permittedArrowDirections = []
        }
        presenter.present(activityController, animated: true)
    }

    private static func messageData(_ body: Any) throws -> Data {
        if let body = body as? String {
            return Data(body.utf8)
        }
        guard JSONSerialization.isValidJSONObject(body) else {
            throw AppleOriginalShareError.invalidRequest(
                "The iOS share request has an unsupported shape."
            )
        }
        return try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
    }
}

enum IronmeshOriginalShareCoordinatorError: LocalizedError {
    case appGroupUnavailable
    case fileProviderUnavailable(String)
    case presentationUnavailable

    var errorDescription: String? {
        switch self {
        case .appGroupUnavailable:
            "The shared iOS File Provider container is unavailable."
        case .fileProviderUnavailable(let message):
            message
        case .presentationUnavailable:
            "Another iOS sheet is already open."
        }
    }
}

func ironmeshIosEmbeddedWebURL(_ url: URL) -> URL {
    guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
        return url
    }
    var queryItems = components.queryItems ?? []
    queryItems.removeAll { $0.name == "embedded_client" }
    queryItems.append(URLQueryItem(name: "embedded_client", value: "ios"))
    components.queryItems = queryItems
    return components.url ?? url
}
