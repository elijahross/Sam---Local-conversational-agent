// First-run onboarding: how to talk to Sam, the macOS permissions it needs (asked one by one,
// each with the reason), connectors and the computer-use opt-in, then a first try. Shown once
// (UserDefaults "onboarded"); reopen from the status-bar dropdown or the island menu.
import AppKit
import AVFoundation
import ServiceManagement
import SwiftUI

@MainActor
final class OnboardingModel: ObservableObject {
    enum Grant { case unknown, granted, denied, working }
    @Published var step = 0
    @Published var mic: Grant = .unknown
    @Published var ax: Grant = .unknown
    @Published var input: Grant = .unknown
    @Published var helper: Grant = .unknown
    @Published var helperNote = ""
    @Published var apps: [String: Grant] = [:]
    @Published var appNotes: [String: String] = [:]
    @Published var computerUse = false
    // Screen context (opt-in): see ContextCapture.
    @Published var highlightAsk = ContextCapture.highlightAsk { didSet { ContextCapture.highlightAsk = highlightAsk } }
    @Published var focusAsk = ContextCapture.focusAsk { didSet { ContextCapture.focusAsk = focusAsk } }
    @Published var notesBackend = "Apple Notes"
    let connectors: ConnectorsStore
    var onConnectors: () -> Void = {}
    var onFinish: () -> Void = {}
    var onFnGranted: () -> Void = {}
    private var timer: Timer?

    static let steps = ["Welcome", "Requirements", "Setup", "AI provider", "Talking", "About you", "Permissions", "Apps", "Connectors", "Try it"]

    // Setup: the one-time runtime install (Services), Sam's voice, start at login.
    @Published var voice = "male"
    @Published var loginItem = SMAppService.mainApp.status == .enabled

    func setVoice(_ v: String) {
        voice = v
        UserDefaults.standard.set("sam-\(v)", forKey: "character")
        Task {
            var req = URLRequest(url: connectors.base.appendingPathComponent("api/settings/voice"), timeoutInterval: 10)
            LocalAuth.sign(&req)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONSerialization.data(withJSONObject: ["voice": v])
            _ = try? await URLSession.shared.data(for: req)
        }
    }

    func loadVoice() async {
        if let j = await LocalAPI(base: connectors.base).get("api/settings/voice") {
            if let v = j["voice"] as? String { voice = v }
            if let m = j["model"] as? String { voiceModel = m }
        }
    }

    /// "light" (0.6B, 1.9 GB) or "steady" (1.7B, 2.4 GB, fewer glitches): saved on the server,
    /// then the app fetches the model if needed and restarts the speech sidecar with it.
    @Published var voiceModel = "light"
    func setVoiceModel(_ m: String, services: Services) {
        voiceModel = m
        Task {
            var req = URLRequest(url: connectors.base.appendingPathComponent("api/settings/voice"), timeoutInterval: 10)
            LocalAuth.sign(&req)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONSerialization.data(withJSONObject: ["model": m])
            _ = try? await URLSession.shared.data(for: req)
            services.applyVoiceModel()
        }
    }

    func setLoginItem(_ on: Bool) {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
        } catch {
            NSLog("Sam: login item: \(error)")
        }
        loginItem = SMAppService.mainApp.status == .enabled
    }

    // AI provider (server: /api/settings/llm): which provider, its key (stored in the Keychain by
    // the server, never shown again), the verified models per task tier.
    // `hasKey` means "ready": a key stored, Ollama running, Claude Code signed in.
    struct ProviderInfo: Identifiable, Equatable { let id: String; let label: String; let hasKey: Bool; let needsKey: Bool; let models: [String]; let installed: [String]; let recommended: [[String]] }
    struct PullInfo: Equatable { let model: String; let percent: Int; let status: String; let done: Bool; let error: String? }
    // Requirements & storage (server: /api/settings/storage).
    @Published var storage: [String: Any] = [:]
    @Published var cleanupEvery = "weekly"
    @Published var keepTasksDays = 90
    @Published var storageNote = ""
    var ramGB: Double { Double(ProcessInfo.processInfo.physicalMemory) / 1_073_741_824 }
    func loadStorage() async {
        guard let j = await LocalAPI(base: connectors.base).get("api/settings/storage") else { return }
        storage = j
        if let st = j["settings"] as? [String: Any] {
            cleanupEvery = st["cleanup_every"] as? String ?? "weekly"
            keepTasksDays = st["keep_tasks_days"] as? Int ?? 90
        }
    }
    func saveStorage() {
        Task {
            _ = await LocalAPI(base: connectors.base).send("api/settings/storage", method: "POST", body: ["cleanup_every": cleanupEvery, "keep_tasks_days": keepTasksDays])
            await loadStorage()
        }
    }
    func cleanupNow() {
        storageNote = "Cleaning up…"
        Task {
            _ = await LocalAPI(base: connectors.base).send("api/settings/storage/cleanup", method: "POST", body: [:])
            await loadStorage()
            storageNote = "Done."
        }
    }
    @Published var providers: [ProviderInfo] = []
    @Published var provider = "minimax"
    @Published var apiKey = ""
    @Published var aiStatus: (text: String, ok: Bool)?
    @Published var aiBusy = false
    /// Ollama: the model for every tier (downloaded on "Use" when missing).
    @Published var ollamaModel = "qwen3.5:9b"
    @Published var pull: PullInfo?

    func loadProviders() async {
        guard let j = await LocalAPI(base: connectors.base).get("api/settings/llm") else { return }
        providers = ((j["providers"] as? [[String: Any]]) ?? []).compactMap { p in
            guard let id = p["id"] as? String else { return nil }
            return ProviderInfo(id: id, label: p["label"] as? String ?? id, hasKey: p["has_key"] as? Bool ?? false, needsKey: p["needs_key"] as? Bool ?? true,
                                models: p["models"] as? [String] ?? [], installed: p["installed"] as? [String] ?? [], recommended: p["recommended"] as? [[String]] ?? [])
        }
        if let cur = j["provider"] as? String, aiStatus == nil { provider = cur }
        if let o = providers.first(where: { $0.id == "ollama" }), o.models.count > 1, !o.models[1].isEmpty, aiStatus == nil { ollamaModel = o.models[1] }
        pull = (j["pull"] as? [String: Any]).map { q in
            let total = (q["total"] as? Int) ?? 0, done = (q["completed"] as? Int) ?? 0
            return PullInfo(model: q["model"] as? String ?? "", percent: total > 0 ? min(100, done * 100 / total) : 0, status: q["status"] as? String ?? "", done: q["done"] as? Bool ?? false, error: q["error"] as? String)
        }
    }

    /// Checks the key with a real call; on success the server saves it and restarts.
    func saveProvider() {
        aiBusy = true
        aiStatus = nil
        var body: [String: Any] = ["provider": provider, "key": apiKey]
        if provider == "ollama" { body["model"] = ollamaModel }
        Task {
            var req = URLRequest(url: connectors.base.appendingPathComponent("api/settings/llm"), timeoutInterval: 90)
            LocalAuth.sign(&req)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONSerialization.data(withJSONObject: body)
            let r = try? await URLSession.shared.data(for: req)
            let j = r.flatMap { (try? JSONSerialization.jsonObject(with: $0.0)) as? [String: Any] }
            if j?["pulling"] as? Bool == true {
                // The model is downloading: follow the progress, then ask again.
                aiStatus = ("Downloading \(ollamaModel)…", true)
                while true {
                    try? await Task.sleep(nanoseconds: 2_000_000_000)
                    await loadProviders()
                    guard let p = pull else { continue }
                    if p.done {
                        if let e = p.error { aiBusy = false; aiStatus = ("Download failed: \(e)", false) } else { saveProvider() }
                        return
                    }
                    aiStatus = ("Downloading \(p.model)… \(p.percent) % (\(p.status))", true)
                }
            }
            aiBusy = false
            if j?["ok"] as? Bool == true {
                let ms = (j?["models"] as? [String]) ?? []
                apiKey = ""
                aiStatus = ("Connected. Light tasks: \(ms.first ?? "—"), standard: \(ms.count > 1 ? ms[1] : "—"), heavy: \(ms.last ?? "—"). Sam restarts its background service to use it.", true)
                try? await Task.sleep(nanoseconds: 4_000_000_000)
                await loadProviders()
            } else {
                aiStatus = ((j?["message"] as? String) ?? "Couldn't reach Sam's service.", false)
            }
        }
    }
    static let appList: [(id: String, name: String, icon: String, why: String)] = [
        ("calendar", "Calendar", "calendar", "read your schedule, add events after your OK"),
        ("reminders", "Reminders", "checklist", "add to-dos and mark them done"),
        ("notes", "Notes", "note.text", "notes for later and your daily journal"),
        ("contacts", "Contacts", "person.crop.circle", "find addresses for emails"),
        ("mail", "Mail", "envelope", "send replies you approved"),
    ]

    init(connectors: ConnectorsStore) { self.connectors = connectors }

    func startPolling() {
        refresh()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in Task { @MainActor in self?.refresh() } }
    }
    func stopPolling() { timer?.invalidate(); timer = nil }

    func refresh() {
        mic = switch AudioIO.micPermission() { case .authorized: .granted; case .denied, .restricted: .denied; default: mic == .working ? .working : .unknown }
        ax = AXIsProcessTrusted() ? .granted : (ax == .working ? .working : .unknown)
        let fnWas = input
        input = Hotkeys.fnAvailable ? .granted : (input == .working ? .working : .unknown)
        if input == .granted && fnWas != .granted { onFnGranted() }
        Task { await refreshHelper(); await refreshConnectors() }
    }

    // MARK: actions

    func askMic() {
        if AudioIO.micPermission() == .denied { openSettings("Privacy_Microphone"); return }
        mic = .working
        AudioIO.requestMic { ok in Task { @MainActor in self.mic = ok ? .granted : .denied } }
    }
    func askAX() {
        ax = .working
        let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        if !AXIsProcessTrustedWithOptions(opts) { openSettings("Privacy_Accessibility") }
    }
    func askInput() {
        input = .working
        Hotkeys.requestFn()
        if !Hotkeys.fnAvailable { openSettings("Privacy_ListenEvent") }
    }
    func openSettings(_ pane: String) {
        if let u = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane)") { NSWorkspace.shared.open(u) }
    }

    /// The "Sam Computer Use" helper: launched in the background, then asked for its own
    /// Accessibility + Screen Recording rights (macOS ties them to the helper, not to Sam).
    func askHelper() {
        helper = .working
        Task {
            if await helperHealth() == nil {
                guard let u = Services.shared.helperURL ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: "one.sam.computeruse") else {
                    helper = .denied; helperNote = "Helper not installed (sidecar/computer/build.sh)"; return
                }
                let c = NSWorkspace.OpenConfiguration(); c.activates = false
                _ = try? await NSWorkspace.shared.openApplication(at: u, configuration: c)
                try? await Task.sleep(for: .seconds(1.5))
            }
            var req = URLRequest(url: URL(string: "http://127.0.0.1:47803/request_access")!, timeoutInterval: 10)
            req.httpMethod = "POST"; LocalAuth.sign(&req); req.httpBody = Data("{}".utf8)
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            _ = try? await URLSession.shared.data(for: req)
            await refreshHelper()
        }
    }
    private func helperHealth() async -> [String: Any]? {
        var req = URLRequest(url: URL(string: "http://127.0.0.1:47803/health")!, timeoutInterval: 2)
        LocalAuth.sign(&req)
        guard let (d, _) = try? await URLSession.shared.data(for: req) else { return nil }
        return (try? JSONSerialization.jsonObject(with: d)) as? [String: Any]
    }
    private func refreshHelper() async {
        guard let h = await helperHealth() else {
            if helper != .working { helper = .unknown; helperNote = "Not running" }
            return
        }
        let ax = h["ax"] as? Bool ?? false, sc = h["screen"] as? Bool ?? false
        helper = ax && sc ? .granted : (helper == .working ? .working : .unknown)
        helperNote = ax && sc ? "Accessibility and Screen Recording on" : "Needs " + [ax ? nil : "Accessibility", sc ? nil : "Screen Recording"].compactMap { $0 }.joined(separator: " and ")
    }

    /// Triggers macOS's Automation prompt for an app now (the server does a harmless count).
    func askApp(_ id: String) {
        apps[id] = .working; appNotes[id] = "Waiting for the macOS prompt…"
        Task {
            var req = URLRequest(url: Remote.base.appendingPathComponent("api/permissions/\(id)"), timeoutInterval: 130)
            req.httpMethod = "POST"; LocalAuth.sign(&req)
            let r = try? await URLSession.shared.data(for: req)
            let j = r.flatMap { (try? JSONSerialization.jsonObject(with: $0.0)) as? [String: Any] }
            let ok = j?["ok"] as? Bool ?? false
            apps[id] = ok ? .granted : .denied
            appNotes[id] = ok ? "Allowed" : (j?["message"] as? String ?? "Sam's server isn't running")
        }
    }

    private func refreshConnectors() async {
        await connectors.refresh()
        if let c = connectors.connectors.first(where: { $0.id == "computer" }) { computerUse = c.status != "off" }
        if let n = connectors.connectors.first(where: { $0.id == "notes" }) { notesBackend = n.detail.hasPrefix("Obsidian") ? "Obsidian" : "Apple Notes" }
    }
    func setComputerUse(_ on: Bool) { computerUse = on; connectors.act("computer", on ? "enable" : "disable") }
    func pickVault() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true; p.canChooseFiles = false
        p.message = "Choose your Obsidian vault folder"
        if p.runModal() == .OK, let u = p.url { connectors.act("notes", "notes_backend", ["backend": "obsidian", "vault": u.path]); notesBackend = "Obsidian" }
    }
    func useAppleNotes() { connectors.act("notes", "notes_backend", ["backend": "apple"]); notesBackend = "Apple Notes" }
}

@MainActor
final class OnboardingController {
    static var done: Bool {
        get { UserDefaults.standard.bool(forKey: "onboarded") }
        set { UserDefaults.standard.set(newValue, forKey: "onboarded") }
    }
    let model: OnboardingModel
    private var window: NSWindow?
    var isOpen: Bool { window?.isVisible ?? false }
    var windowForTests: NSWindow? { window }

    /// Tests share the app's defaults domain: they never mark the real onboarding as done.
    var testMode = false

    init(connectors: ConnectorsStore) {
        model = OnboardingModel(connectors: connectors)
        model.onFinish = { [weak self] in
            if self?.testMode == false { OnboardingController.done = true }
            self?.hide()
        }
    }

    func show(step: Int = 0, screen: NSScreen? = nil) {
        if window == nil {
            let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 920, height: 640), styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
            w.titlebarAppearsTransparent = true
            w.titleVisibility = .hidden
            w.backgroundColor = NSColor(Theme.bgDeep)
            w.title = "Welcome to Sam"
            w.titlebarAppearsTransparent = true
            w.titleVisibility = .hidden
            w.isMovableByWindowBackground = true
            w.isReleasedWhenClosed = false
            w.isRestorable = false
            w.appearance = NSAppearance(named: .darkAqua)
            let blur = NSVisualEffectView()
            blur.material = .underWindowBackground; blur.blendingMode = .behindWindow; blur.state = .active
            let host = NSHostingView(rootView: OnboardingView(m: model).tint(Theme.accent))
            host.translatesAutoresizingMaskIntoConstraints = false
            blur.addSubview(host)
            NSLayoutConstraint.activate([host.leadingAnchor.constraint(equalTo: blur.leadingAnchor), host.trailingAnchor.constraint(equalTo: blur.trailingAnchor),
                                         host.topAnchor.constraint(equalTo: blur.topAnchor), host.bottomAnchor.constraint(equalTo: blur.bottomAnchor)])
            w.contentView = blur
            w.center()
            NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: w, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.model.stopPolling() }
            }
            window = w
        }
        if let s = screen, let w = window { w.setFrameOrigin(NSPoint(x: s.frame.midX - w.frame.width / 2, y: s.frame.midY - w.frame.height / 2)) }
        model.step = step
        model.startPolling()
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    func hide() { model.stopPolling(); window?.orderOut(nil) }
}

// MARK: - Views

struct OnboardingView: View {
    @ObservedObject var m: OnboardingModel
    @StateObject private var friend: FriendStore
    @Environment(\.accessibilityReduceMotion) private var reduce
    init(m: OnboardingModel) {
        self.m = m
        _friend = StateObject(wrappedValue: FriendStore(base: m.connectors.base))
    }

    static let stepIcons = ["sparkle", "cpu", "arrow.down.circle", "brain.head.profile", "waveform", "person.crop.circle", "lock.shield", "app.badge", "square.grid.2x2", "play.circle"]

    var body: some View {
        HStack(spacing: 0) {
            rail.frame(width: 232).padding(.top, 40).padding(.bottom, 24).padding(.leading, 22)
            VStack(spacing: 0) {
                stepContent
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                    .padding(28)
                    .transition(.opacity.combined(with: .move(edge: .trailing)))
                    .id(m.step)
                Divider().overlay(Theme.fill)
                footer.padding(.horizontal, 24).padding(.vertical, 16)
            }
            .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
            .padding(.vertical, 22).padding(.trailing, 22).padding(.leading, 14)
        }
        .frame(width: 920, height: 640)
        .background(backdrop)
        .foregroundStyle(Theme.text)
        .environment(\.colorScheme, .dark)
        .animation(Theme.content(reduce), value: m.step)
        .onChange(of: m.step) { _, _ in friend.flush() }
    }

    /// A deep blue-violet field with two soft glows: calm, premium, the island's own palette.
    private var backdrop: some View {
        AppBackdrop()
    }

    /// The steps as a list: done ones ticked, the current one on a glass pill, the rest dimmed.
    private var rail: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 10) {
                SamLogoView(size: 18, color: Theme.text)
                    .frame(width: 36, height: 36).samGlass(in: Circle())
                VStack(alignment: .leading, spacing: 1) {
                    Text("Sam").font(Theme.h3)
                    Text("Setup").font(Theme.caption).foregroundStyle(Theme.text3)
                }
            }
            .padding(.bottom, 22).padding(.leading, 6)
            ForEach(Array(OnboardingModel.steps.enumerated()), id: \.offset) { i, name in
                let current = i == m.step, done = i < m.step
                Button { m.step = i } label: {
                    HStack(spacing: 10) {
                        Group {
                            if i == 0 && !done {
                                SamLogoView(size: 14, color: current ? Theme.accent : Theme.text3)
                            } else {
                                Image(systemName: done ? "checkmark.circle.fill" : Self.stepIcons[min(i, Self.stepIcons.count - 1)])
                                    .font(Theme.body.weight(.semibold))
                                    .foregroundStyle(done ? Theme.good : (current ? Theme.accent : Theme.text3))
                            }
                        }
                        .frame(width: 20)
                        Text(name).font(.system(size: 13, weight: current ? .semibold : .regular)).foregroundStyle(current ? Theme.text : (done ? Theme.text2 : Theme.text3))
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .background(current ? AnyView(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill)) : AnyView(Color.clear))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(current ? [.isButton, .isSelected] : .isButton)
            }
            Spacer()
            Text("Step \(m.step + 1) of \(OnboardingModel.steps.count)").font(Theme.caption).foregroundStyle(Theme.text3).padding(.leading, 12)
        }
    }

    @ViewBuilder private var stepContent: some View {
        Group {
            Group {
                switch m.step {
                case 0: welcome
                case 1: RequirementsStepView(m: m)
                case 2: setupStep
                case 3: aiStep
                case 4: talking
                case 5: AboutYouView(f: friend)
                case 6: permissions
                case 7: appsStep
                case 8: connectorsStep
                default: tryIt
                }
            }
        }
    }

    private var footer: some View {
        HStack {
            if m.step > 0 { Button("Back") { m.step -= 1 }.samButton() }
            Spacer()
            if m.step < OnboardingModel.steps.count - 1 {
                Button("Skip setup") { friend.flush(); m.onFinish() }.buttonStyle(.plain).foregroundStyle(Theme.text3).padding(.trailing, 8)
                Button { friend.flush(); m.step += 1 } label: { Label("Continue", systemImage: "arrow.right").labelStyle(.titleAndIcon) }.samButton(prominent: true).keyboardShortcut(.defaultAction)
            } else {
                Button("Start using Sam") { m.onFinish() }.samButton(prominent: true).keyboardShortcut(.defaultAction)
            }
        }
    }

    private func header(_ title: String, _ sub: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(Theme.display).tracking(-0.4)
            Text(sub).font(Theme.lead).foregroundStyle(Theme.text2).lineSpacing(2).fixedSize(horizontal: false, vertical: true)
        }
        .padding(.bottom, 18)
    }

    // 1
    private var welcome: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 14) {
                SamLogoView(size: 32, color: Theme.text)
                    .frame(width: 64, height: 64).samGlass(in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                header("Meet Sam", "A voice assistant that lives in your notch and works in the background while you keep working.")
            }
            feature("waveform", "Talk, don't type", "Ask anything; Sam answers out loud and shows cards for weather, events and results.")
            feature("scope", "Point and ask", "Point at a window, a PDF or highlighted text and say \u{201C}reply to this\u{201D}, \u{201C}find gaps in this essay\u{201D}, \u{201C}make an event from this\u{201D}.")
            feature("square.stack.3d.up", "Background tasks", "Research, replies, forms and app work run off-screen. The menu-bar icon shows progress; nothing outgoing happens without your OK.")
            feature("text.cursor", "Dictation", "Hold a key, speak, release: clean text lands where your cursor is.")
        }
    }

    // Setup
    private var setupStep: some View {
        SetupStepView(m: m, services: Services.shared)
    }

    // AI provider
    private var aiStep: some View {
        VStack(alignment: .leading, spacing: 12) {
            header("Your AI provider", "Sam thinks with the provider you choose: a cloud provider with your own API key, Ollama with a model that runs on this Mac (nothing leaves it), or the Claude Code app signed in here. For each job it picks the cheapest model that can do it well: small for scoring and checks, standard for planning and answers, large only for long research write-ups.")
            Picker("Provider", selection: $m.provider) {
                ForEach(m.providers) { p in Text(p.label + (p.hasKey ? "  ✓" : "")).tag(p.id) }
            }
            .pickerStyle(.radioGroup).labelsHidden()
            if let p = m.providers.first(where: { $0.id == m.provider }) {
                if p.needsKey {
                    Text("Models: \(p.models.first ?? "") (light) · \(p.models.count > 1 ? p.models[1] : "") (standard) · \(p.models.last ?? "") (heavy)")
                        .font(Theme.caption).foregroundStyle(Theme.text3)
                    HStack(spacing: 8) {
                        SecureField(p.hasKey ? "Key saved — paste a new one to replace it" : "Paste your \(p.label) API key", text: $m.apiKey)
                            .textFieldStyle(.roundedBorder).frame(maxWidth: 420)
                            .accessibilityLabel("API key")
                        Button(m.aiBusy ? "Checking…" : (p.hasKey && m.apiKey.isEmpty ? "Use \(p.label)" : "Check & use")) { m.saveProvider() }
                            .samButton(prominent: true).disabled(m.aiBusy || (m.apiKey.isEmpty && !p.hasKey))
                    }
                } else if p.id == "ollama" {
                    Text("Free and private: the model runs on this Mac through Ollama, with no account and no connection needed. Sam's own voice models take about 5 GB of memory, so pick a model that fits the \(Int(m.ramGB)) GB in this Mac. Answers come slower than from a cloud model.")
                        .font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                    if !p.hasKey {
                        Label("Ollama isn't running. Install it from ollama.com, open it, then come back here.", systemImage: "exclamationmark.triangle.fill")
                            .font(Theme.caption).foregroundStyle(Theme.warn)
                    }
                    Picker("Model", selection: $m.ollamaModel) {
                        ForEach(ollamaChoices(p), id: \.0) { c in Text(c.1).tag(c.0) }
                    }
                    .pickerStyle(.menu).frame(maxWidth: 520)
                    Button(m.aiBusy ? "Working…" : (p.installed.contains(m.ollamaModel) ? "Use \(m.ollamaModel)" : "Download & use \(m.ollamaModel)")) { m.saveProvider() }
                        .samButton(prominent: true).disabled(m.aiBusy || !p.hasKey)
                } else if p.id == "claude-code" {
                    Text("Uses the Claude Code app that is signed in on this Mac, with your own Claude subscription: nothing to paste. Each answer starts a short-lived headless Claude Code run with every tool switched off. This is for you on your own Mac; it isn't a way to let other people use your subscription.")
                        .font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                    if !p.hasKey {
                        Label(p.installed.isEmpty ? "Claude Code isn't installed. Get it at claude.ai/code, then sign in." : "Claude Code isn't signed in: open Terminal, run claude, then /login.", systemImage: "exclamationmark.triangle.fill")
                            .font(Theme.caption).foregroundStyle(Theme.warn)
                    }
                    Text("Models: \(p.models.first ?? "") (light) · \(p.models.count > 1 ? p.models[1] : "") (standard) · \(p.models.last ?? "") (heavy)")
                        .font(Theme.caption).foregroundStyle(Theme.text3)
                    Button(m.aiBusy ? "Checking…" : "Use Claude Code") { m.saveProvider() }
                        .samButton(prominent: true).disabled(m.aiBusy || !p.hasKey)
                }
            }
            if let s = m.aiStatus {
                Label(s.text, systemImage: s.ok ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                    .font(Theme.caption).foregroundStyle(s.ok ? Theme.good : Theme.warn).fixedSize(horizontal: false, vertical: true)
            }
            if m.providers.first(where: { $0.id == m.provider })?.needsKey ?? true {
                Text("The key is stored in your Mac's Keychain and only sent to that provider.").font(Theme.caption).foregroundStyle(Theme.text3)
            }
        }
        .task { await m.loadProviders() }
    }

    /// Ollama's model menu: the recommended models (with their size) first, then anything else installed.
    private func ollamaChoices(_ p: OnboardingModel.ProviderInfo) -> [(String, String)] {
        var out: [(String, String)] = p.recommended.compactMap { r in
            guard let name = r.first else { return nil }
            return (name, (r.count > 1 ? r[1] : name) + (p.installed.contains(name) ? "  ✓ installed" : ""))
        }
        for i in p.installed where !out.contains(where: { $0.0 == i }) { out.append((i, "\(i)  ✓ installed")) }
        if !out.contains(where: { $0.0 == m.ollamaModel }) { out.append((m.ollamaModel, m.ollamaModel)) }
        return out
    }

    // 2
    private var talking: some View {
        VStack(alignment: .leading, spacing: 12) {
            header("How to talk to Sam", "No wake word. Use whichever feels natural.")
            keyRow(["⌥", "Space"], "Hold to talk", "Release when you're done. Tap once instead to talk hands-free; tap again or press Esc to stop.")
            keyRow(["fn"], "Hover the notch, then hold fn to talk", "fn only works while the island is open, so volume and brightness keys stay yours. Double-tap fn there for hands-free. Needs Input Monitoring (next step).")
            keyRow(["Hover"], "Move the pointer to the notch", "The island opens and listens.")
            keyRow(["⌥", "⇧", "Space"], "Hold to dictate", "Or fn + Shift. Release and the text is inserted into the focused field; \u{201C}new line\u{201D} and \u{201C}new paragraph\u{201D} work.")
            keyRow(["\u{201C}this\u{201D}"], "Point while you speak", "The window under your pointer (or your selection) goes with the request: \u{201C}reply to Markus I'd be glad to meet Monday\u{201D}.")
        }
    }

    // 3
    private var permissions: some View {
        VStack(alignment: .leading, spacing: 10) {
            header("Permissions", "macOS asks once for each. Sam only uses them for what's listed.")
            permRow("mic", "Microphone", "To hear you. Audio is transcribed on this Mac.", m.mic, action: m.askMic)
            permRow("hand.point.up.left", "Accessibility", "To insert dictation, and to read a selection or window if you switch that on (next steps).", m.ax, action: m.askAX)
            permRow("keyboard", "Input Monitoring", "Only for the fn key (⌥Space works without it).", m.input, action: m.askInput, optional: true)
            permRow("cursorarrow.motionlines", "Computer use helper", m.helperNote.isEmpty ? "Accessibility and Screen Recording for the \u{201C}Sam Computer Use\u{201D} helper, if you want Sam to operate apps." : m.helperNote, m.helper, action: m.askHelper, optional: true)
            Text("After switching a permission on in System Settings, come back here; the list updates by itself.").font(Theme.caption).foregroundStyle(Theme.text3).padding(.top, 4)
        }
    }

    // 4
    private var appsStep: some View {
        VStack(alignment: .leading, spacing: 10) {
            header("Your apps", "Sam works with your Mac's own apps. Each Allow shows the macOS prompt now, so it doesn't interrupt you later.")
            ForEach(OnboardingModel.appList, id: \.id) { a in
                permRow(a.icon, a.name, m.appNotes[a.id].map { $0 == "Allowed" ? a.why : $0 } ?? a.why, m.apps[a.id] ?? .unknown, action: { m.askApp(a.id) }, optional: true, label: "Allow")
            }
        }
    }

    // 5
    private var connectorsStep: some View {
        VStack(alignment: .leading, spacing: 12) {
            header("Connectors", "Choose what Sam may use. You can change all of this later in Connectors.")
            card {
                VStack(alignment: .leading, spacing: 10) {
                    HStack {
                        Image(systemName: "text.cursor").foregroundStyle(Theme.accent).frame(width: 28)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Highlight and ask").font(Theme.body.weight(.semibold))
                            Text("Select text in any app, then ask: \u{201C}what does this mean?\u{201D} Sam gets the selection with your question.").font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer()
                        Toggle("", isOn: $m.highlightAsk).toggleStyle(.switch).labelsHidden().accessibilityLabel("Highlight and ask")
                    }
                    HStack {
                        Image(systemName: "macwindow").foregroundStyle(Theme.accent).frame(width: 28)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Focus and ask").font(Theme.body.weight(.semibold))
                            Text("Ask about the window in front or under the pointer: \u{201C}summarize this.\u{201D} Sam gets its title, address and text.").font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer()
                        Toggle("", isOn: $m.focusAsk).toggleStyle(.switch).labelsHidden().accessibilityLabel("Focus and ask")
                    }
                    if m.highlightAsk || m.focusAsk {
                        Label(ContextCapture.leakWarning, systemImage: "exclamationmark.triangle.fill")
                            .font(Theme.caption).foregroundStyle(Theme.warn).fixedSize(horizontal: false, vertical: true)
                    } else {
                        Text("Both off: Sam never reads your screen. You can switch them on later in Sam's menu.").font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                }
            }
            card {
                HStack {
                    Image(systemName: "note.text").foregroundStyle(Theme.accent).frame(width: 28)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Notes & journal").font(Theme.body.weight(.semibold))
                        Text("Where \u{201C}note this\u{201D} and \u{201C}add to my journal\u{201D} go: \(m.notesBackend)").font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                    Spacer()
                    Button("Apple Notes") { m.useAppleNotes() }.samButton(prominent: m.notesBackend == "Apple Notes").controlSize(.small)
                    Button("Obsidian…") { m.pickVault() }.samButton(prominent: m.notesBackend == "Obsidian").controlSize(.small)
                }
            }
            card {
                HStack {
                    Image(systemName: "cursorarrow.motionlines").foregroundStyle(Theme.accent).frame(width: 28)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Computer use").font(Theme.body.weight(.semibold))
                        Text("Sam may operate apps in the background (Calculator, TextEdit, Pages…). Every goal asks you first; Terminal and Keychain are off-limits.").font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer()
                    Toggle("", isOn: Binding(get: { m.computerUse }, set: { m.setComputerUse($0) })).toggleStyle(.switch).labelsHidden().accessibilityLabel("Computer use")
                }
            }
            card {
                HStack {
                    Image(systemName: "puzzlepiece.extension").foregroundStyle(Theme.accent).frame(width: 28)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Email, WhatsApp, logins, folders, integrations").font(Theme.body.weight(.semibold))
                        Text("Connect an email account, link WhatsApp or Telegram, grant folders, add GitHub / Notion / Slack…").font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                    Spacer()
                    Button("Open Connectors") { m.onConnectors() }.samButton().controlSize(.small)
                }
            }
            if let msg = m.connectors.message, !msg.ok { Text(msg.text).font(Theme.caption).foregroundStyle(Theme.warn) }
        }
    }

    // 6
    private var tryIt: some View {
        VStack(alignment: .leading, spacing: 12) {
            header("Try it", "Hold ⌥Space and say one of these, then let go.")
            ForEach(["\u{201C}What's the weather tomorrow?\u{201D}", "\u{201C}Remind me to call Anna at five.\u{201D}", "\u{201C}Add to my journal: finished the onboarding.\u{201D}"] + (m.focusAsk ? ["Point at an article: \u{201C}summarize this.\u{201D}"] : []), id: \.self) { s in
                HStack(spacing: 10) {
                    Image(systemName: "quote.bubble").foregroundStyle(Theme.accent)
                    Text(s).font(Theme.lead)
                }
                .padding(.vertical, 10).padding(.horizontal, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
            }
            Text("This tour is in the menu-bar dropdown (?) whenever you want it again.").font(Theme.caption).foregroundStyle(Theme.text3).padding(.top, 6)
        }
    }

    // MARK: pieces

    private func feature(_ icon: String, _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Image(systemName: icon).font(.system(size: 16, weight: .semibold)).foregroundStyle(Theme.accent).frame(width: 30)
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(Theme.body.weight(.semibold))
                Text(text).font(Theme.body).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func keyRow(_ keys: [String], _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            HStack(spacing: 4) {
                ForEach(keys, id: \.self) { k in
                    Text(k).font(.system(size: 12, weight: .semibold, design: .rounded)).padding(.horizontal, 8).padding(.vertical, 4)
                        .background(RoundedRectangle(cornerRadius: 6).fill(Theme.fillSelected))
                        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Theme.strokeStrong))
                }
            }
            .frame(width: 150, alignment: .leading)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(Theme.body.weight(.semibold))
                Text(text).font(Theme.caption).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, 4)
    }

    private func permRow(_ icon: String, _ name: String, _ why: String, _ g: OnboardingModel.Grant, action: @escaping () -> Void, optional: Bool = false, label: String = "Grant") -> some View {
        card {
            HStack(spacing: 12) {
                Image(systemName: icon).font(.system(size: 15, weight: .medium)).foregroundStyle(Theme.accent).frame(width: 28)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(name).font(Theme.body.weight(.semibold))
                        if optional { Text("optional").font(.system(size: 9, weight: .semibold)).foregroundStyle(Theme.text3) }
                    }
                    Text(why).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(2)
                }
                Spacer()
                switch g {
                case .granted: Label("On", systemImage: "checkmark.circle.fill").font(Theme.caption).foregroundStyle(Theme.good).labelStyle(.titleAndIcon)
                case .working: ProgressView().controlSize(.small)
                case .denied: Button("Open Settings") { action() }.samButton().controlSize(.small)
                case .unknown: Button(label, action: action).samButton(prominent: !optional).controlSize(.small)
                }
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(name)
    }

    private func card<C: View>(@ViewBuilder _ c: () -> C) -> some View {
        c().padding(.vertical, 10).padding(.horizontal, 12)
            .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
            .overlay(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).stroke(Theme.stroke))
    }
}


/// Setup step: the one-time install with progress, Sam's voice, start at login.
struct RequirementsStepView: View {
    @ObservedObject var m: OnboardingModel

    private func gb(_ v: Any?) -> String {
        let b = (v as? Double) ?? Double((v as? Int) ?? 0)
        return b >= 1_073_741_824 ? String(format: "%.1f GB", b / 1_073_741_824) : String(format: "%.0f MB", b / 1_048_576)
    }

    var body: some View {
        let sizes = m.storage["sizes"] as? [String: Any] ?? [:]
        let chrome = sizes["chrome"] as? Bool
        let ramOK = m.ramGB >= 16, ramLow = m.ramGB < 12
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 6) {
                Text("What Sam needs").font(Theme.h1)
                Text("Everything runs on this Mac. Check these three things once, and decide how long Sam keeps what it did.")
                    .font(Theme.lead).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            }
            .padding(.bottom, 2)
            row(ok: chrome == true, warn: chrome == false, icon: "globe",
                title: "Google Chrome",
                text: chrome == false ? "Not found in /Applications. Sam uses Chrome for web research, listings, web chats and logins. Install it from google.com/chrome, then continue." : "Found. Sam drives its own Chrome windows on an invisible workspace display; your own Chrome stays untouched.")
            row(ok: ramOK, warn: ramLow, icon: "memorychip",
                title: String(format: "Memory: %.0f GB", m.ramGB),
                text: ramLow ? "16 GB is recommended. Sam's voice, speech and memory models take about 4 GB while active (about 1 GB idle); with less, heavy work runs slower and alongside big apps it may unload models." : "16 GB or more: the voice and memory models (about 4 GB active, 1 GB idle) fit comfortably next to your apps.")
            row(ok: (sizes["free"] as? Double ?? 0) > 10 * 1_073_741_824, warn: (sizes["free"] as? Double ?? 0) < 5 * 1_073_741_824, icon: "internaldrive",
                title: "Storage: \(gb(sizes["free"])) free",
                text: "Models \(gb(sizes["models"])) and the runtime \(gb(sizes["runtime"])) are installed once. Sam's own data (\(gb(sizes["data_total"]))) grows with use: browser sessions and caches \(gb(sizes["browser"])), research and reports \(gb(sizes["artifacts"])), task history \(gb(sizes["history"])), memory and knowledge \(gb(sizes["memory"])). Automations and monitors keep their results in their own stores; their routine runs leave the task history after two weeks.")
            Divider().padding(.vertical, 2)
            HStack(spacing: 18) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Clean up temporary files").font(Theme.body.weight(.semibold))
                    Picker("", selection: Binding(get: { m.cleanupEvery }, set: { m.cleanupEvery = $0; m.saveStorage() })) {
                        Text("Daily").tag("daily"); Text("Weekly").tag("weekly"); Text("Monthly").tag("monthly"); Text("Never").tag("never")
                    }
                    .pickerStyle(.segmented).labelsHidden().frame(width: 300)
                    Text("Browser caches (sessions stay) and the working folders of finished tasks.").font(Theme.caption).foregroundStyle(Theme.text3)
                }
                VStack(alignment: .leading, spacing: 4) {
                    Text("Keep finished tasks").font(Theme.body.weight(.semibold))
                    Picker("", selection: Binding(get: { m.keepTasksDays }, set: { m.keepTasksDays = $0; m.saveStorage() })) {
                        Text("30 days").tag(30); Text("90 days").tag(90); Text("1 year").tag(365); Text("Forever").tag(0)
                    }
                    .pickerStyle(.segmented).labelsHidden().frame(width: 300)
                    Text("Task history and reports older than this are removed.").font(Theme.caption).foregroundStyle(Theme.text3)
                }
            }
            HStack(spacing: 10) {
                Button("Clean up now") { m.cleanupNow() }.samButton().controlSize(.small)
                if let st = m.storage["settings"] as? [String: Any], let last = st["last_cleanup"] as? Double {
                    Text("Last cleanup \(Date(timeIntervalSince1970: last / 1000).formatted(date: .abbreviated, time: .shortened)), freed \(gb(st["last_freed_bytes"]))").font(Theme.caption).foregroundStyle(Theme.text3)
                }
                Text(m.storageNote).font(Theme.caption).foregroundStyle(Theme.text3)
                Spacer()
                Button("Uninstall Sam…") { Uninstaller.confirmAndRun() }.buttonStyle(.plain).font(Theme.caption).foregroundStyle(Theme.text3)
                    .help("Removes the app and everything it keeps on this Mac: models, runtime, data, logs, caches, preferences, Keychain items, login item")
            }
        }
        .task { await m.loadStorage() }
    }

    private func row(ok: Bool, warn: Bool, icon: String, title: String, text: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: ok ? "checkmark.circle.fill" : (warn ? "exclamationmark.triangle.fill" : icon)).foregroundStyle(ok ? Theme.good : (warn ? Theme.warn : Theme.text2)).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(Theme.body.weight(.semibold))
                Text(text).font(Theme.caption).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

struct SetupStepView: View {
    @ObservedObject var m: OnboardingModel
    @ObservedObject var services: Services

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Setting up Sam").font(Theme.h1)
                Text("Sam's voice, speech and memory run on this Mac. The first time, Sam downloads them once (about 3 GB). You can continue with the next steps meanwhile.")
                    .font(Theme.lead).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            }
            .padding(.bottom, 6)
            switch services.setup {
            case .ready:
                Label("Voice and memory are ready.", systemImage: "checkmark.circle.fill").foregroundStyle(Theme.good)
            case .running(let what):
                VStack(alignment: .leading, spacing: 6) {
                    ProgressView(value: services.progress).tint(Theme.accent).frame(maxWidth: 480)
                    Text(what).font(Theme.caption).foregroundStyle(Theme.text2)
                }
            case .failed(let why):
                VStack(alignment: .leading, spacing: 6) {
                    Label("Setup stopped: \(why)", systemImage: "exclamationmark.triangle.fill").font(Theme.caption).foregroundStyle(Theme.warn).fixedSize(horizontal: false, vertical: true)
                    Button("Try again") { services.install() }.samButton(prominent: true)
                }
            case .needed, .unknown:
                Button("Start setup") { services.install() }.samButton(prominent: true)
            }
            Divider().padding(.vertical, 6)
            Text("Sam's voice").font(Theme.body.weight(.semibold))
            Picker("Voice", selection: Binding(get: { m.voice }, set: { m.setVoice($0) })) {
                Text("Male").tag("male")
                Text("Female").tag("female")
            }
            .pickerStyle(.segmented).labelsHidden().frame(width: 220)
            Text("Voice model").font(Theme.body.weight(.semibold)).padding(.top, 4)
            Picker("Voice model", selection: Binding(get: { m.voiceModel }, set: { m.setVoiceModel($0, services: services) })) {
                Text("Tiny").tag("tiny")
                Text("Light").tag("light")
                Text("Steady").tag("steady")
            }
            .pickerStyle(.segmented).labelsHidden().frame(width: 300)
            Text(m.voiceModel == "steady" ? "Steady: the larger voice (2.4 GB) with almost no glitches; the first word comes a little later."
                 : m.voiceModel == "tiny" ? "Tiny: the smallest voice (0.3 GB, very little memory), instant but plainer; English only."
                                          : "Light: the smaller voice (1.9 GB), quick to answer; a rare glitch on very short words.")
                .font(Theme.caption).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            if let st = services.voiceModelStatus {
                Label(st, systemImage: st.hasPrefix("Couldn't") ? "exclamationmark.triangle.fill" : "arrow.down.circle").font(Theme.caption)
                    .foregroundStyle(st.hasPrefix("Couldn't") ? Theme.warn : Theme.text2)
            }
            Toggle("Open Sam when I log in", isOn: Binding(get: { m.loginItem }, set: { m.setLoginItem($0) }))
                .toggleStyle(.switch)
        }
        .task {
            await m.loadVoice()
            if services.bundled && !services.runtimeReady && services.setup != .ready {
                if case .running = services.setup {} else { services.install() }
            }
        }
    }
}
