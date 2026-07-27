import AppleCore
import SwiftUI
import UIKit
import UniformTypeIdentifiers
import WebKit
#if canImport(Vision)
import Vision
#endif
#if canImport(VisionKit)
import VisionKit
#endif

@main
struct IronmeshIosApp: App {
    @StateObject private var model = IronmeshBrowserModel()

    var body: some Scene {
        WindowGroup {
            if let session = IronmeshUiTestWebUiSession.fromLaunchEnvironment() {
                if IronmeshUiTestWebUiSession.embeddedSurface == .galleryMap {
                    IronmeshGalleryMapContent(
                        session: session,
                        isStarting: false,
                        statusMessage: "",
                        onStart: {}
                    )
                } else {
                    IronmeshHostedWebView(session: session)
                }
            } else {
                IronmeshIosRootView()
                    .environmentObject(model)
                    .task {
                        model.activate()
                    }
            }
        }
    }
}

private enum IronmeshUiTestWebUiSession {
    static var embeddedSurface: IronmeshEmbeddedSurface? {
        IronmeshEmbeddedSurface(
            rawValue: ProcessInfo.processInfo.environment["IRONMESH_UI_TEST_EMBEDDED_SURFACE"] ?? ""
        )
    }

    static func fromLaunchEnvironment() -> AppleWebUiSession? {
        guard let url = ProcessInfo.processInfo.environment["IRONMESH_UI_TEST_WEB_UI_URL"],
              let data = try? JSONSerialization.data(
                  withJSONObject: [
                      "url": url,
                      "authorization": "gallery-map-simulator-test",
                  ]
              ),
              let responseJSON = String(data: data, encoding: .utf8) else {
            return nil
        }
        return try? AppleWebUiSession(responseJSON: responseJSON)
    }
}

private struct IronmeshIosRootView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        Group {
            if model.shouldShowOnboarding {
                IronmeshOnboardingGateView()
            } else {
                IronmeshMainShellView()
            }
        }
        .sheet(
            item: Binding(
                get: { model.webUIPresentation },
                set: { presentation in
                    if presentation == nil {
                        model.closeWebUI()
                    }
                }
            )
        ) { presentation in
            IronmeshHostedWebView(session: presentation.session)
        }
        .onChange(of: scenePhase) { phase in
            if phase == .active {
                model.notifyForegrounded()
            } else {
                model.closeWebUI()
                model.closeGalleryMap()
            }
        }
    }
}

private struct IronmeshMainShellView: View {
    var body: some View {
        TabView {
            IronmeshHomeView()
                .tabItem {
                    Label("Home", systemImage: "house")
                }

            IronmeshLibraryView()
                .tabItem {
                    Label("Library", systemImage: "books.vertical")
                }

            IronmeshGalleryMapView()
                .tabItem {
                    Label("Gallery Map", systemImage: "map")
                }

            IronmeshFilesView()
                .tabItem {
                    Label("Files", systemImage: "folder.badge.gearshape")
                }

            IronmeshSettingsView()
                .tabItem {
                    Label("Settings", systemImage: "gearshape")
                }
        }
    }
}

private enum IronmeshEmbeddedSurface: String {
    case galleryMap = "gallery_map"
}

private struct IronmeshGalleryMapView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel

    var body: some View {
        IronmeshGalleryMapContent(
            session: model.galleryMapPresentation?.session,
            isStarting: model.isBusy,
            statusMessage: model.statusText,
            onStart: model.openGalleryMap
        )
        .onDisappear {
            model.closeGalleryMap()
        }
    }
}

private struct IronmeshGalleryMapContent: View {
    let session: AppleWebUiSession?
    let isStarting: Bool
    let statusMessage: String
    let onStart: () -> Void

    var body: some View {
        if let session, let galleryMapSession = galleryMapWebUiSession(from: session) {
            IronmeshHostedWebView(session: galleryMapSession)
                .ignoresSafeArea()
        } else {
            galleryMapStartCard
        }
    }

    private var galleryMapStartCard: some View {
        VStack {
            Spacer()
            IronmeshCard(
                title: "Gallery Map",
                subtitle: isStarting
                    ? "Starting the embedded gallery map."
                    : "Open the shared gallery map directly inside the app."
            ) {
                if !statusMessage.isEmpty {
                    Text(statusMessage)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }

                if isStarting {
                    ProgressView()
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Button("Open gallery map", action: onStart)
                        .buttonStyle(.borderedProminent)
                }
            }
            Spacer()
        }
        .padding(16)
        .background(Color(uiColor: .systemGroupedBackground))
        .accessibilityIdentifier("ironmesh-gallery-map-start")
    }
}

private func galleryMapWebUiSession(from session: AppleWebUiSession) -> AppleWebUiSession? {
    guard var components = URLComponents(url: session.url, resolvingAgainstBaseURL: false) else {
        return nil
    }
    var queryItems = components.queryItems ?? []
    queryItems.removeAll { $0.name == "embedded" || $0.name == "embedded_client" }
    queryItems.append(URLQueryItem(name: "embedded", value: IronmeshEmbeddedSurface.galleryMap.rawValue))
    queryItems.append(URLQueryItem(name: "embedded_client", value: "ios"))
    components.queryItems = queryItems

    guard let url = components.url,
          let data = try? JSONSerialization.data(
              withJSONObject: [
                  "url": url.absoluteString,
                  "authorization": session.authorization,
              ]
          ),
          let responseJSON = String(data: data, encoding: .utf8) else {
        return nil
    }
    return try? AppleWebUiSession(responseJSON: responseJSON)
}

private struct IronmeshOnboardingGateView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @State private var showsScanner = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 18) {
                    IronmeshHeroCard(
                        title: "Connect this iPhone",
                        body: "Import a connection bootstrap bundle. Files integration stays separate, so the shell can stay honest about what the app can already browse."
                    )

                    IronmeshCard(title: "Quick setup", subtitle: "Import the routes and trust material advertised by your cluster.") {
                        TextField("Device label (optional)", text: draftBinding(\.deviceLabel))
                            .textInputAutocapitalization(.words)

                        if model.draft.requiresEnrollment {
                            VStack(alignment: .leading, spacing: 8) {
                                Text("This bootstrap bundle must enroll the device before the app can browse the cluster.")
                                    .font(.footnote)
                                    .foregroundStyle(.secondary)

                                Button("Enroll device") {
                                    model.enrollDevice(completesOnboarding: true)
                                }
                                .buttonStyle(.borderedProminent)
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                        }

                        IronmeshMultilineField(
                            title: "Bootstrap bundle",
                            text: draftBinding(\.bootstrapInput),
                            prompt: "Paste bootstrap JSON or import it from a QR code."
                        )

                        VStack(spacing: 12) {
                            HStack {
                                Button("Scan QR") {
                                    showsScanner = true
                                }
                                .buttonStyle(.bordered)

                                Button("Use defaults") {
                                    model.resetToBundleDefaults()
                                }
                                .buttonStyle(.bordered)

                                Spacer()
                            }

                            if !model.draft.requiresEnrollment {
                                HStack {
                                    Spacer()

                                    Button("Continue") {
                                        model.completeOnboarding()
                                    }
                                    .buttonStyle(.borderedProminent)
                                }
                            }
                        }
                    }

                    IronmeshCard(title: "What happens next", subtitle: model.draft.enrollmentSummary) {
                        IronmeshKeyValueRow(label: "Connection", value: model.draft.setupSummary)
                        IronmeshKeyValueRow(label: "Identity", value: model.draft.identitySummary)
                        IronmeshKeyValueRow(label: "Device", value: model.draft.enrolledDeviceID.nilIfBlank ?? "Not enrolled yet")
                        IronmeshKeyValueRow(label: "Domain", value: model.draft.domainDisplayName)
                        Text("If the camera is unavailable, the QR sheet still accepts pasted payloads.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }

                    if let lastErrorMessage = model.lastErrorMessage {
                        IronmeshCard(title: "Last issue", subtitle: "Resolve this before continuing.") {
                            Text(lastErrorMessage)
                                .foregroundStyle(.red)
                                .font(.footnote)
                        }
                    }
                }
                .padding(16)
            }
            .background(Color(uiColor: .systemGroupedBackground))
            .navigationTitle("Onboarding")
        }
        .sheet(isPresented: $showsScanner) {
            IronmeshScannerSheet { payload in
                model.applyScannedCode(payload)
            }
        }
    }

    private func draftBinding(_ keyPath: WritableKeyPath<IronmeshConnectionDraft, String>) -> Binding<String> {
        Binding(
            get: { model.draft[keyPath: keyPath] },
            set: { model.draft[keyPath: keyPath] = $0 }
        )
    }
}

private struct IronmeshHomeView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @State private var showsFilesPicker = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 18) {
                    IronmeshHeroCard(
                        title: model.healthHeadline,
                        body: model.healthSummary,
                        tone: toneForHome(model: model)
                    ) {
                        HStack {
                            Button("Refresh root") {
                                model.refresh()
                            }
                            .buttonStyle(.borderedProminent)

                            Button("Open Web UI") {
                                model.openWebUI()
                            }
                            .buttonStyle(.bordered)

                            Button("Open Files") {
                                showsFilesPicker = true
                            }
                            .buttonStyle(.bordered)
                        }
                    }

                    LazyVGrid(
                        columns: [
                            GridItem(.flexible(), spacing: 12),
                            GridItem(.flexible(), spacing: 12),
                        ],
                        spacing: 12
                    ) {
                        IronmeshMetricTile(label: "Root items", value: "\(model.items.count)")
                        IronmeshMetricTile(label: "Folders", value: "\(model.rootDirectoryCount)")
                        IronmeshMetricTile(label: "Files", value: "\(model.rootFileCount)")
                        IronmeshMetricTile(label: "Last success", value: relativeDate(model.lastSuccessfulConnectionAt))
                    }

                    IronmeshCard(title: "Connection overview", subtitle: model.draft.enrollmentSummary) {
                        IronmeshKeyValueRow(label: "Status", value: model.statusText)
                        IronmeshKeyValueRow(label: "Connection", value: model.draft.normalizedConnectionInput ?? "Not configured")
                        IronmeshKeyValueRow(label: "Role", value: model.connectionDiagnostics?.connectionName ?? "ios app shell")
                        IronmeshKeyValueRow(label: "Identity", value: model.draft.identitySummary)
                        if let enrolledDeviceID = model.draft.enrolledDeviceID.nilIfBlank {
                            IronmeshKeyValueRow(label: "Device ID", value: enrolledDeviceID)
                        }
                        IronmeshKeyValueRow(label: "Domain", value: model.domainState.title)
                        if let filesSelectionSummary = model.filesSelectionSummary {
                            IronmeshKeyValueRow(label: "Files handoff", value: filesSelectionSummary)
                        }
                    }

                    IronmeshCard(title: "Recent activity", subtitle: "The latest app-side actions and checks.") {
                        if model.recentActions.isEmpty {
                            Text("No actions recorded yet.")
                                .foregroundStyle(.secondary)
                        } else {
                            ForEach(model.recentActions) { action in
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(action.title)
                                        .font(.headline)
                                    Text(action.detail)
                                        .font(.footnote)
                                        .foregroundStyle(.secondary)
                                    Text(timestamp(action.timestamp))
                                        .font(.caption2.monospacedDigit())
                                        .foregroundStyle(.tertiary)
                                }
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(.vertical, 2)
                            }
                        }
                    }
                }
                .padding(16)
            }
            .background(Color(uiColor: .systemGroupedBackground))
            .navigationTitle("Home")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    IronmeshTitleLatencyToolbarItem()
                }
            }
        }
        .sheet(isPresented: $showsFilesPicker) {
            IronmeshFilesHandoffPicker { url in
                model.noteFilesSelection(url)
            }
        }
    }

    private func toneForHome(model: IronmeshBrowserModel) -> IronmeshHeroTone {
        if model.lastErrorMessage != nil && model.lastSuccessfulConnectionAt == nil {
            return .error
        }
        if model.lastErrorMessage != nil {
            return .warning
        }
        if model.lastSuccessfulConnectionAt != nil {
            return .good
        }
        return .neutral
    }
}

private struct IronmeshLibraryView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @State private var selectedFilePath: String?
    @State private var showsFilesPicker = false
    @State private var section: IronmeshLibrarySection = .browse

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                Picker("Library section", selection: $section) {
                    Label("Browse", systemImage: "folder").tag(IronmeshLibrarySection.browse)
                    Label("Photos", systemImage: "photo.on.rectangle").tag(IronmeshLibrarySection.photos)
                }
                .pickerStyle(.segmented)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)

                if section == .browse {
                    ScrollView {
                        VStack(spacing: 18) {
                            IronmeshCard(title: "Remote browser", subtitle: "Browse the remote library directly from the app.") {
                                IronmeshKeyValueRow(label: "Current path", value: displayPath(model.currentPath))
                                HStack {
                                    Button("Root") {
                                        model.browse(path: "")
                                    }
                                    .buttonStyle(.bordered)

                                    Button("Up") {
                                        model.navigateUp()
                                    }
                                    .buttonStyle(.bordered)
                                    .disabled(model.currentPath.isEmpty)

                                    Button("Refresh") {
                                        model.refreshCurrentDirectory()
                                    }
                                    .buttonStyle(.borderedProminent)

                                    Button("Open Files") {
                                        showsFilesPicker = true
                                    }
                                    .buttonStyle(.bordered)
                                }
                            }

                            if !model.breadcrumbs.isEmpty {
                                IronmeshCard(title: "Breadcrumbs") {
                                    ScrollView(.horizontal, showsIndicators: false) {
                                        HStack(spacing: 8) {
                                            Button("/") {
                                                model.browse(path: "")
                                            }
                                            .buttonStyle(.bordered)

                                            ForEach(model.breadcrumbs) { breadcrumb in
                                                Button(breadcrumb.label) {
                                                    model.browse(path: breadcrumb.path)
                                                }
                                                .buttonStyle(.bordered)
                                            }
                                        }
                                    }
                                }
                            }

                            if model.currentItems.isEmpty {
                                IronmeshCard(title: "No items", subtitle: "Refresh the current path or adjust the connection in Settings.") {
                                    Text("This directory is empty or has not been loaded yet.")
                                        .foregroundStyle(.secondary)
                                }
                            } else {
                                if !model.libraryDirectories.isEmpty {
                                    IronmeshCard(title: "Folders") {
                                        ForEach(model.libraryDirectories, id: \.identifier.serialized) { item in
                                            Button {
                                                model.browse(path: item.path)
                                            } label: {
                                                IronmeshBrowserRow(item: item)
                                            }
                                            .buttonStyle(.plain)
                                        }
                                    }
                                }

                                if !model.libraryFiles.isEmpty {
                                    IronmeshCard(title: "Files") {
                                        ForEach(model.libraryFiles, id: \.identifier.serialized) { item in
                                            Button {
                                                selectedFilePath = item.path
                                            } label: {
                                                IronmeshBrowserRow(item: item)
                                            }
                                            .buttonStyle(.plain)
                                        }
                                    }
                                }
                            }
                        }
                        .padding(16)
                    }
                    .task {
                        if model.currentItems.isEmpty {
                            model.refreshCurrentDirectory()
                        }
                    }
                } else {
                    IronmeshGalleryView()
                }
            }
            .background(Color(uiColor: .systemGroupedBackground))
            .navigationTitle("Library")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    IronmeshTitleLatencyToolbarItem()
                }
            }
        }
        .sheet(
            isPresented: Binding(
                get: { selectedFilePath != nil },
                set: { isPresented in
                    if !isPresented {
                        selectedFilePath = nil
                    }
                }
            )
        ) {
            if let item = model.item(at: selectedFilePath) {
                IronmeshFileInspectorView(item: item)
            } else {
                Text("Selected file is no longer available.")
                    .padding()
            }
        }
        .sheet(isPresented: $showsFilesPicker) {
            IronmeshFilesHandoffPicker { url in
                model.noteFilesSelection(url)
            }
        }
    }
}

private enum IronmeshLibrarySection {
    case browse
    case photos
}

struct IronmeshConnectionPathsView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel

    var body: some View {
        List {
            if let error = model.connectionRoutesErrorMessage {
                Section {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }
            }

            if let snapshot = model.connectionRouteSnapshot {
                overview(snapshot)

                Section("Ranked paths") {
                    ForEach(Array(snapshot.rankedEndpoints.enumerated()), id: \.element.id) { rank, endpoint in
                        IronmeshConnectionPathRow(
                            endpoint: endpoint,
                            rank: rank + 1,
                            snapshotTimestamp: snapshot.generatedAtUnixMs
                        )
                    }
                }
            } else if model.isRefreshingConnectionRoutes {
                Section {
                    HStack {
                        Spacer()
                        ProgressView("Evaluating connection paths…")
                        Spacer()
                    }
                }
            } else {
                Section {
                    VStack(spacing: 8) {
                        Image(systemName: "point.3.connected.trianglepath.dotted")
                            .font(.title)
                            .foregroundStyle(.secondary)
                        Text("No connection paths yet")
                            .font(.headline)
                        Text("Evaluate routes to inspect direct and relay candidates.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
                }
            }
        }
        .navigationTitle("Connection paths")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    model.refreshConnectionPaths()
                } label: {
                    if model.isRefreshingConnectionRoutes {
                        ProgressView()
                    } else {
                        Label("Re-evaluate", systemImage: "arrow.clockwise")
                    }
                }
                .disabled(model.isRefreshingConnectionRoutes)
            }
        }
        .task {
            if model.connectionRouteSnapshot == nil && !model.isRefreshingConnectionRoutes {
                model.refreshConnectionPaths()
            }
        }
    }

    @ViewBuilder
    private func overview(_ snapshot: AppleConnectionRouteSnapshot) -> some View {
        Section("Overview") {
            IronmeshKeyValueRow(
                label: "Selected path",
                value: snapshot.activeEndpoint?.connectionDisplayName ?? "None"
            )
            if let explanation = snapshot.activeEndpoint?.connectionExplanation {
                Text(explanation)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            IronmeshKeyValueRow(label: "Candidates", value: "\(snapshot.endpoints.count)")
            IronmeshKeyValueRow(label: "Direct", value: "\(snapshot.directEndpointCount)")
            IronmeshKeyValueRow(label: "Relay", value: "\(snapshot.relayEndpointCount)")
            IronmeshKeyValueRow(
                label: "Evaluated",
                value: unixMillisecondsTimestamp(snapshot.generatedAtUnixMs)
            )
            Text("Routes are evaluated only when this view opens or you explicitly refresh it.")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }
}

private struct IronmeshConnectionPathRow: View {
    let endpoint: AppleConnectionRouteEndpoint
    let rank: Int
    let snapshotTimestamp: UInt64

    private var coolingDown: Bool {
        endpoint.isCoolingDown(atUnixMs: currentUnixMilliseconds)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Label(endpoint.connectionDisplayName, systemImage: pathIcon)
                    .font(.headline)
                Spacer()
                Text("#\(rank)")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }

            Text(endpoint.locator)
                .font(.footnote.monospaced())
                .textSelection(.enabled)

            if let explanation = endpoint.connectionExplanation {
                Text(explanation)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 8) {
                if endpoint.active {
                    IronmeshConnectionPathBadge(title: "Active", color: .green)
                }
                if endpoint.backgroundProbeInFlight {
                    IronmeshConnectionPathBadge(title: "Probing", color: .blue)
                }
                if coolingDown {
                    IronmeshConnectionPathBadge(title: "Cooldown", color: .orange)
                }
                if !endpoint.active && !endpoint.backgroundProbeInFlight && !coolingDown {
                    IronmeshConnectionPathBadge(title: "Standby", color: .gray)
                }
            }

            Group {
                IronmeshKeyValueRow(label: "EWMA latency", value: milliseconds(endpoint.ewmaLatencyMs))
                IronmeshKeyValueRow(label: "EWMA throughput", value: bytesPerSecond(endpoint.ewmaThroughputBytesPerSec))
                IronmeshKeyValueRow(label: "Score", value: String(format: "%.2f", endpoint.score))
                IronmeshKeyValueRow(label: "Successes", value: "\(endpoint.totalSuccesses)")
                IronmeshKeyValueRow(
                    label: "Failures",
                    value: "\(endpoint.consecutiveFailures) consecutive, \(endpoint.totalFailures) total"
                )
                IronmeshKeyValueRow(label: "Bootstrap rank", value: "\(endpoint.bootstrapRank)")
                if let holePunchingMode = endpoint.holePunchingMode {
                    IronmeshKeyValueRow(label: "QUIC path", value: holePunchingMode.capitalized)
                }
                if let targetNodeId = endpoint.targetNodeId {
                    IronmeshKeyValueRow(label: "Target node", value: targetNodeId)
                }
                IronmeshKeyValueRow(
                    label: "Last measurement",
                    value: unixMillisecondsTimestamp(endpoint.lastMeasurementUnixMs)
                )
                IronmeshKeyValueRow(
                    label: "Last success",
                    value: unixMillisecondsTimestamp(endpoint.lastSuccessUnixMs)
                )
                IronmeshKeyValueRow(
                    label: "Last failure",
                    value: unixMillisecondsTimestamp(endpoint.lastFailureUnixMs)
                )
                IronmeshKeyValueRow(
                    label: "Last probe",
                    value: unixMillisecondsTimestamp(endpoint.lastBackgroundProbeUnixMs)
                )
                if let circuitOpenUntilUnixMs = endpoint.circuitOpenUntilUnixMs {
                    IronmeshKeyValueRow(
                        label: "Cooldown until",
                        value: unixMillisecondsTimestamp(circuitOpenUntilUnixMs)
                    )
                }
                if let lastError = endpoint.lastError {
                    IronmeshKeyValueRow(label: "Last error", value: lastError)
                }
            }
            .font(.footnote)
        }
        .padding(.vertical, 6)
        .accessibilityElement(children: .contain)
        .accessibilityHint("Ranked at \(rank), snapshot \(unixMillisecondsTimestamp(snapshotTimestamp))")
    }

    private var pathIcon: String {
        if endpoint.usesRelayPath {
            return "arrow.triangle.branch"
        }
        switch endpoint.pathKind {
        case .directHTTPS:
            return "lock.shield"
        case .directQUIC:
            return endpoint.isDirectQuicHolePunched
                ? "point.3.connected.trianglepath.dotted"
                : "bolt.horizontal"
        case .relayTunnel:
            return "arrow.triangle.branch"
        case .unknown:
            return "questionmark.circle"
        }
    }
}

private struct IronmeshConnectionPathBadge: View {
    let title: String
    let color: Color

    var body: some View {
        Text(title)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(color.opacity(0.12), in: Capsule())
    }
}

struct IronmeshTitleLatencyToolbarItem: View {
    @EnvironmentObject private var model: IronmeshBrowserModel

    var body: some View {
        if model.titleLatencyStatus.state != "disabled" {
            Text(label)
                .font(.caption2.monospacedDigit().weight(.semibold))
                .foregroundStyle(color)
                .accessibilityLabel(accessibilityLabel)
        }
    }

    private var connectionPrefix: String {
        switch model.titleLatencyStatus.connectionType {
        case "direct":
            return "D"
        case "direct_quic":
            return "Q"
        case "relay":
            return "R"
        default:
            return "?"
        }
    }

    private var label: String {
        switch model.titleLatencyStatus.state {
        case "success":
            let milliseconds = Int((model.titleLatencyStatus.latencyMs ?? 0).rounded())
            return "\(connectionPrefix) \(milliseconds) ms"
        case "pending":
            return "\(connectionPrefix) ..."
        default:
            return "\(connectionPrefix) --"
        }
    }

    private var color: Color {
        if model.titleLatencyStatus.state == "failed" {
            return .red
        }
        switch model.titleLatencyStatus.connectionType {
        case "direct":
            return .green
        case "direct_quic":
            return .teal
        case "relay":
            return .orange
        default:
            return .secondary
        }
    }

    private var accessibilityLabel: String {
        switch model.titleLatencyStatus.state {
        case "success":
            return "\(connectionDescription) latency \(label)"
        case "pending":
            return "Measuring \(connectionDescription) latency"
        default:
            return "\(connectionDescription) latency unavailable"
        }
    }

    private var connectionDescription: String {
        switch model.titleLatencyStatus.connectionType {
        case "direct":
            return "direct connection"
        case "direct_quic":
            return "direct QUIC connection through NAT"
        case "relay":
            return "relay connection"
        default:
            return "connection"
        }
    }
}

private struct IronmeshSettingsView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @State private var showsScanner = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Connection") {
                    if let normalizedConnectionInput = model.draft.normalizedConnectionInput {
                        IronmeshInlineNote(text: normalizedConnectionInput)
                    } else {
                        IronmeshInlineNote(text: "Import a connection bootstrap bundle below.")
                    }

                    if model.draft.requiresEnrollment {
                        IronmeshInlineNote(
                            text: "This bootstrap bundle requires device enrollment before the app can reconnect."
                        )
                    }

                    Button(model.draft.requiresEnrollment ? "Go to enrollment" : "Apply and reconnect") {
                        model.applyConnectionSettings()
                    }
                }

                Section("Identity") {
                    TextField("Device label (optional)", text: draftBinding(\.deviceLabel))
                        .textInputAutocapitalization(.words)

                    if let enrolledDeviceID = model.draft.enrolledDeviceID.nilIfBlank {
                        IronmeshInlineNote(text: "Enrolled device: \(enrolledDeviceID)")
                    }

                    IronmeshMultilineEditor(
                        title: "Client identity JSON",
                        text: draftBinding(\.clientIdentityJSON),
                        prompt: "Optional JSON identity material."
                    )

                    IronmeshMultilineEditor(
                        title: "Server CA PEM",
                        text: draftBinding(\.serverCAPem),
                        prompt: "Optional CA override for bootstrap-advertised HTTPS routes."
                    )

                    if model.draft.hasClientIdentity || model.draft.serverCAPem.nilIfBlank != nil {
                        Button("Clear identity material", role: .destructive) {
                            model.clearIdentity()
                        }
                    }
                }

                Section("Title latency") {
                    Toggle(
                        "Measure connection latency",
                        isOn: Binding(
                            get: { model.titleLatencyMonitorSettings.enabled },
                            set: { enabled in
                                model.updateTitleLatencyMonitorEnabled(enabled)
                            }
                        )
                    )

                    if model.titleLatencyMonitorSettings.enabled {
                        Stepper(
                            "Check period: \(model.titleLatencyMonitorSettings.periodSeconds) seconds",
                            value: Binding(
                                get: { Int(model.titleLatencyMonitorSettings.periodSeconds) },
                                set: { model.updateTitleLatencyMonitorPeriodSeconds(UInt64($0)) }
                            ),
                            in: 5...300,
                            step: 5
                        )
                        Text("The compact title result uses D for direct and R for relay. The color follows the current route type.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }

                Section("Bootstrap") {
                    IronmeshMultilineEditor(
                        title: "Bootstrap bundle",
                        text: draftBinding(\.bootstrapInput),
                        prompt: "Paste bootstrap JSON here or import it from a QR code."
                    )

                    if model.draft.requiresEnrollment {
                        IronmeshInlineNote(
                            text: "Enrollment will mint client identity material for this bootstrap bundle."
                        )
                    }

                    HStack {
                        Button("Scan QR") {
                            showsScanner = true
                        }
                        .buttonStyle(.bordered)

                        if model.draft.hasBootstrapPayload {
                            Button(model.draft.requiresEnrollment ? "Enroll device" : "Re-enroll device") {
                                model.enrollDevice()
                            }
                            .buttonStyle(.borderedProminent)

                            Button("Clear bootstrap", role: .destructive) {
                                model.draft.bootstrapInput = ""
                            }
                        }
                    }
                }

                Section("Advanced") {
                    TextField("Domain display name", text: draftBinding(\.domainDisplayName))
                    TextField("Domain identifier", text: draftBinding(\.domainIdentifier))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()

                    Button("Open web UI") {
                        model.openWebUI()
                    }

                    Button("Restore bundled defaults") {
                        model.resetToBundleDefaults()
                    }

                    Button("Clear app setup", role: .destructive) {
                        model.clearAppSetup()
                    }
                }

                Section("Provider note") {
                    Text(model.filesIntegrationNote)
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Settings")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    IronmeshTitleLatencyToolbarItem()
                }
            }
        }
        .sheet(isPresented: $showsScanner) {
            IronmeshScannerSheet { payload in
                model.applyScannedCode(payload)
            }
        }
    }

    private func draftBinding(_ keyPath: WritableKeyPath<IronmeshConnectionDraft, String>) -> Binding<String> {
        Binding(
            get: { model.draft[keyPath: keyPath] },
            set: { model.draft[keyPath: keyPath] = $0 }
        )
    }
}

private struct IronmeshFileInspectorView: View {
    @EnvironmentObject private var model: IronmeshBrowserModel
    @Environment(\.dismiss) private var dismiss

    let item: AppleBridgeItem

    @State private var preview: IronmeshFilePreviewResult?
    @State private var isLoading = true

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 18) {
                    IronmeshCard(title: item.displayName, subtitle: item.kind == .directory ? "Directory" : "File") {
                        IronmeshKeyValueRow(label: "Path", value: item.path)
                        IronmeshKeyValueRow(label: "Identifier", value: item.identifier.serialized)
                        IronmeshKeyValueRow(label: "Revision", value: item.revisionHint ?? "Unavailable")
                        IronmeshKeyValueRow(label: "Size", value: byteCount(item.sizeBytes))
                        IronmeshKeyValueRow(label: "Modified", value: unixTimestamp(item.modifiedAtUnix))
                    }

                    IronmeshCard(title: "Preview", subtitle: "A pragmatic view into the selected file.") {
                        if isLoading {
                            ProgressView()
                                .frame(maxWidth: .infinity, alignment: .center)
                                .padding(.vertical, 16)
                        } else if let preview {
                            switch preview.payload {
                            case .text(let text):
                                Text(text)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .font(.system(.footnote, design: .monospaced))
                                    .textSelection(.enabled)
                            case .image(let data):
                                if let image = UIImage(data: data) {
                                    Image(uiImage: image)
                                        .resizable()
                                        .scaledToFit()
                                        .frame(maxWidth: .infinity)
                                } else {
                                    Text("Image data could not be decoded.")
                                        .foregroundStyle(.secondary)
                                }
                            case .binary(let message):
                                Text(message)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
                .padding(16)
            }
            .background(Color(uiColor: .systemGroupedBackground))
            .navigationTitle("File")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Done") {
                        dismiss()
                    }
                }
            }
            .task {
                preview = await model.loadPreview(for: item)
                isLoading = false
            }
        }
    }
}

struct IronmeshCard<Content: View>: View {
    let title: String
    var subtitle: String?
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.headline)
                if let subtitle, !subtitle.isEmpty {
                    Text(subtitle)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
        .background(
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .fill(Color(uiColor: .secondarySystemGroupedBackground))
        )
    }
}

struct IronmeshHeroCard<Content: View>: View {
    let title: String
    let message: String
    var tone: IronmeshHeroTone = .neutral
    @ViewBuilder var content: Content

    init(
        title: String,
        body: String,
        tone: IronmeshHeroTone = .neutral,
        @ViewBuilder content: () -> Content = { EmptyView() }
    ) {
        self.title = title
        message = body
        self.tone = tone
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(title)
                .font(.title2.weight(.semibold))
            Text(message)
                .font(.body)
                .foregroundStyle(.secondary)
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(18)
        .background(
            RoundedRectangle(cornerRadius: 24, style: .continuous)
                .fill(tone.background)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 24, style: .continuous)
                .stroke(tone.border, lineWidth: 1)
        )
    }
}

private struct IronmeshMetricTile: View {
    let label: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label)
                .font(.footnote)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.title3.weight(.semibold))
        }
        .frame(maxWidth: .infinity, minHeight: 88, alignment: .leading)
        .padding(16)
        .background(
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .fill(Color(uiColor: .secondarySystemGroupedBackground))
        )
    }
}

struct IronmeshBrowserRow: View {
    let item: AppleBridgeItem

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: item.kind == .directory ? "folder" : "doc")
                .foregroundStyle(item.kind == .directory ? .blue : .primary)
                .frame(width: 18, height: 18)

            VStack(alignment: .leading, spacing: 4) {
                Text(item.displayName)
                    .foregroundStyle(.primary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(item.path.isEmpty ? "/" : item.path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if item.kind == .file {
                    Text(byteCount(item.sizeBytes))
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }

            Spacer(minLength: 0)

            Image(systemName: item.kind == .directory ? "chevron.right" : "eye")
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 4)
    }
}

struct IronmeshKeyValueRow: View {
    let label: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.footnote)
                .textSelection(.enabled)
        }
    }
}

private struct IronmeshMultilineField: View {
    let title: String
    let text: Binding<String>
    let prompt: String

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.subheadline.weight(.medium))
            IronmeshMultilineEditor(title: title, text: text, prompt: prompt)
        }
    }
}

private struct IronmeshMultilineEditor: View {
    let title: String
    let text: Binding<String>
    let prompt: String

    var body: some View {
        ZStack(alignment: .topLeading) {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(Color(uiColor: .tertiarySystemGroupedBackground))

            if text.wrappedValue.isEmpty {
                Text(prompt)
                    .foregroundStyle(.tertiary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 14)
            }

            TextEditor(text: text)
                .scrollContentBackground(.hidden)
                .frame(minHeight: 120)
                .padding(8)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }
}

private struct IronmeshInlineNote: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.footnote)
            .foregroundStyle(.secondary)
            .textSelection(.enabled)
    }
}

struct IronmeshFilesHandoffPicker: UIViewControllerRepresentable {
    let onPick: (URL) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onPick: onPick)
    }

    func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
        let controller = UIDocumentPickerViewController(
            forOpeningContentTypes: [.folder, .item],
            asCopy: false
        )
        controller.delegate = context.coordinator
        controller.allowsMultipleSelection = false
        controller.shouldShowFileExtensions = true
        return controller
    }

    func updateUIViewController(_ uiViewController: UIDocumentPickerViewController, context: Context) {
    }

    final class Coordinator: NSObject, UIDocumentPickerDelegate {
        private let onPick: (URL) -> Void

        init(onPick: @escaping (URL) -> Void) {
            self.onPick = onPick
        }

        func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
            guard let url = urls.first else {
                return
            }
            onPick(url)
        }
    }
}

private struct IronmeshHostedWebView: UIViewControllerRepresentable {
    let session: AppleWebUiSession

    func makeCoordinator() -> Coordinator {
        Coordinator(origin: session.url)
    }

    func makeUIViewController(context: Context) -> UIViewController {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.accessibilityIdentifier = "ironmesh-hosted-web-ui"
        webView.allowsLinkPreview = false
        webView.navigationDelegate = context.coordinator
        context.coordinator.webView = webView

        var request = URLRequest(url: session.url)
        request.setValue(session.authorization, forHTTPHeaderField: "X-IronMesh-Web-Ui-Session")
        webView.load(request)

        let controller = UIViewController()
        controller.view = webView
        return controller
    }

    func updateUIViewController(_ uiViewController: UIViewController, context: Context) {
        _ = uiViewController
        _ = context
    }

    final class Coordinator: NSObject, WKNavigationDelegate {
        private let origin: URL
        weak var webView: WKWebView?

        init(origin: URL) {
            self.origin = origin
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
        ) {
            guard let candidate = navigationAction.request.url,
                  candidate.scheme == origin.scheme,
                  candidate.host == origin.host,
                  candidate.port == origin.port else {
                decisionHandler(.cancel)
                return
            }
            decisionHandler(.allow)
        }
    }
}

private struct IronmeshScannerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var manualPayload = ""
    @State private var scannerMessage = "Scan a QR code or paste the payload below."

    let onScan: (String) -> Void

    var body: some View {
        NavigationStack {
            VStack(spacing: 16) {
                scannerSurface
                    .frame(maxWidth: .infinity, maxHeight: 260)
                    .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))

                Text(scannerMessage)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)

                IronmeshMultilineEditor(
                    title: "Manual payload",
                    text: $manualPayload,
                    prompt: "Paste connection bootstrap JSON."
                )

                Button("Use pasted payload") {
                    guard !manualPayload.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                        return
                    }
                    onScan(manualPayload)
                    dismiss()
                }
                .buttonStyle(.borderedProminent)
                .frame(maxWidth: .infinity, alignment: .leading)

                Spacer()
            }
            .padding(16)
            .navigationTitle("Import QR")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Close") {
                        dismiss()
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var scannerSurface: some View {
        #if canImport(VisionKit)
        if #available(iOS 16.0, *), DataScannerViewController.isSupported {
            IronmeshLiveScanner(
                onScan: { payload in
                    onScan(payload)
                    dismiss()
                },
                onFailure: { message in
                    scannerMessage = message
                }
            )
        } else {
            scannerFallback
        }
        #else
        scannerFallback
        #endif
    }

    private var scannerFallback: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 20, style: .continuous)
                .fill(Color(uiColor: .secondarySystemGroupedBackground))
            VStack(spacing: 10) {
                Image(systemName: "qrcode.viewfinder")
                    .font(.system(size: 32))
                Text("Live scanning is unavailable on this device. Paste the payload instead.")
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
            }
            .padding(24)
        }
    }
}

#if canImport(VisionKit)
@available(iOS 16.0, *)
private struct IronmeshLiveScanner: UIViewControllerRepresentable {
    let onScan: (String) -> Void
    let onFailure: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onScan: onScan, onFailure: onFailure)
    }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let controller = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: true,
            isPinchToZoomEnabled: true,
            isGuidanceEnabled: true,
            isHighlightingEnabled: true
        )
        controller.delegate = context.coordinator
        return controller
    }

    func updateUIViewController(_ uiViewController: DataScannerViewController, context: Context) {
        guard !uiViewController.isScanning else {
            return
        }

        do {
            try uiViewController.startScanning()
        } catch {
            onFailure(error.localizedDescription)
        }
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onScan: (String) -> Void
        private let onFailure: (String) -> Void
        private var hasCompletedScan = false

        init(onScan: @escaping (String) -> Void, onFailure: @escaping (String) -> Void) {
            self.onScan = onScan
            self.onFailure = onFailure
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            guard let payload = addedItems.compactMap(extractPayload).first else {
                return
            }
            deliver(payload)
        }

        func dataScanner(_ dataScanner: DataScannerViewController, didTapOn item: RecognizedItem) {
            guard let payload = extractPayload(item) else {
                return
            }
            deliver(payload)
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable
        ) {
            onFailure(error.localizedDescription)
        }

        private func deliver(_ payload: String) {
            guard !hasCompletedScan else {
                return
            }
            hasCompletedScan = true
            onScan(payload)
        }

        private func extractPayload(_ item: RecognizedItem) -> String? {
            switch item {
            case .barcode(let barcode):
                return barcode.payloadStringValue
            case .text(let text):
                return text.transcript
            @unknown default:
                return nil
            }
        }
    }
}
#endif

enum IronmeshHeroTone {
    case neutral
    case good
    case warning
    case error

    var background: Color {
        switch self {
        case .neutral:
            return Color(uiColor: .secondarySystemGroupedBackground)
        case .good:
            return Color.green.opacity(0.16)
        case .warning:
            return Color.orange.opacity(0.16)
        case .error:
            return Color.red.opacity(0.16)
        }
    }

    var border: Color {
        switch self {
        case .neutral:
            return Color.primary.opacity(0.08)
        case .good:
            return Color.green.opacity(0.35)
        case .warning:
            return Color.orange.opacity(0.35)
        case .error:
            return Color.red.opacity(0.35)
        }
    }
}

private func timestamp(_ date: Date?) -> String {
    guard let date else {
        return "Unavailable"
    }

    return IronmeshDateFormatters.timestamp.string(from: date)
}

private func relativeDate(_ date: Date?) -> String {
    guard let date else {
        return "Never"
    }

    return IronmeshDateFormatters.relative.localizedString(for: date, relativeTo: Date())
}

private func unixTimestamp(_ timestamp: Int64?) -> String {
    guard let timestamp else {
        return "Unavailable"
    }

    return IronmeshDateFormatters.timestamp.string(from: Date(timeIntervalSince1970: TimeInterval(timestamp)))
}

private func unixMillisecondsTimestamp(_ timestamp: UInt64?) -> String {
    guard let timestamp else {
        return "Unavailable"
    }

    return IronmeshDateFormatters.timestamp.string(
        from: Date(timeIntervalSince1970: TimeInterval(timestamp) / 1_000)
    )
}

private var currentUnixMilliseconds: UInt64 {
    UInt64(Date().timeIntervalSince1970 * 1_000)
}

private func milliseconds(_ value: Double?) -> String {
    guard let value, value.isFinite else {
        return "Unavailable"
    }
    return String(format: "%.1f ms", value)
}

private func bytesPerSecond(_ value: Double?) -> String {
    guard let value, value.isFinite, value >= 0, value <= Double(Int64.max) else {
        return "Unavailable"
    }
    let formatter = ByteCountFormatter()
    formatter.countStyle = .file
    return "\(formatter.string(fromByteCount: Int64(value.rounded())))/s"
}

private func byteCount(_ bytes: Int64?) -> String {
    guard let bytes else {
        return "Unavailable"
    }

    let formatter = ByteCountFormatter()
    formatter.countStyle = .file
    return formatter.string(fromByteCount: bytes)
}

private enum IronmeshDateFormatters {
    static var timestamp: DateFormatter {
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .short
        return formatter
    }

    static var relative: RelativeDateTimeFormatter {
        RelativeDateTimeFormatter()
    }
}
