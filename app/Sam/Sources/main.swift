// Sam: the island app. Wires activation (hover / ⌥Space / fn), audio, the WebSocket to the
// Rust server and the island state machine together.
import AppKit
import Combine
import SwiftUI

@MainActor
final class Controller {
    let model = IslandModel()
    let audio = AudioIO()
    let socket: SamSocket
    var notch: NotchController!
    let keys = Hotkeys()
    let tasks: TaskStore
    var statusBar: StatusBar!
    var overview: OverviewController!
    var connectors: ConnectorsController!
    var onboarding: OnboardingController!
    /// Drop-down notifications (tests switch them on explicitly: the user keeps working meanwhile).
    var toasts = true
    /// Test mode keeps the announce path off (it opens the island on its own); the E2E announce test turns it on.
    var announceInTests = false
    var testMode = ProcessInfo.processInfo.environment["SAM_TEST"] == "1" || CommandLine.arguments.contains("--test")
    private var hoverTask: Task<Void, Never>?
    /// The approved action the "Working on it…" badge stands for (cleared when it finished).
    private var chipAction: String?
    private var chipTimeout: Task<Void, Never>?
    /// Hands-free closes (mic off) when nobody speaks: after opening (`idleFirst`) and after a
    /// reply while waiting for a follow-up (`idleFollowUp`). Server VAD ends each utterance.
    var idleFirst: TimeInterval = 8
    var idleFollowUp: TimeInterval = 6
    private var idleTask: Task<Void, Never>?
    private var bag = Set<AnyCancellable>()
    private var collapseTask: Task<Void, Never>?
    private var disconnectTask: Task<Void, Never>?
    private var clickMonitor: Any?
    private var replyStarted = false
    private var agentDone = false
    private(set) var lastFrameSent = Date.distantPast

    init() {
        let url = URL(string: ProcessInfo.processInfo.environment["SAM_WS"] ?? "ws://127.0.0.1:47800/ws")!
        socket = SamSocket(url: url)
        var http = URLComponents(url: url, resolvingAgainstBaseURL: false)!
        http.scheme = url.scheme == "wss" ? "https" : "http"; http.path = "/"
        tasks = TaskStore(base: http.url!)
        Remote.base = http.url!
        notch = NotchController(model: model)
        wire()
        if testMode { toasts = false; idleFirst = 60; idleFollowUp = 60 }
        let open: (String) -> Void = { path in
            if path.hasPrefix("http"), let u = URL(string: path) { NSWorkspace.shared.open(u) } else { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }
        }
        statusBar = StatusBar(store: tasks)
        overview = OverviewController(store: tasks)
        overview.onOpenFile = open
        connectors = ConnectorsController(store: ConnectorsStore(base: http.url!))
        let showConnectors: () -> Void = { [weak self] in self?.connectors.show() }
        statusBar.onConnectors = showConnectors
        overview.onConnectors = showConnectors
        notch.onConnectors = { [weak self] in self?.close(); self?.connectors.show() }
        statusBar.onOverview = { [weak self] id in self?.overview.show(task: id) }
        onboarding = OnboardingController(connectors: connectors.store)
        onboarding.testMode = testMode
        onboarding.model.onConnectors = showConnectors
        onboarding.model.onFnGranted = { [weak self] in self?.keys.enableFn() }
        let showTour: () -> Void = { [weak self] in self?.close(); self?.onboarding.show() }
        statusBar.onTour = showTour
        statusBar.onVoiceOff = { [weak self] in self?.model.voiceOff.toggle() }
        notch.onTour = showTour
        notch.onOverview = { [weak self] id in self?.dismissToast(); self?.overview.show(task: id) }
        notch.onToastDismiss = { [weak self] in self?.dismissToast() }
        // Live activity around the notch while tasks run / wait.
        tasks.$tasks.sink { [weak self] _ in
            Task { @MainActor in
                guard let self else { return }
                self.model.liveRunning = self.tasks.running
                self.model.liveNeeds = self.tasks.needsAction
                // The badge goes as soon as the approved action is no longer being carried out.
                if let aid = self.chipAction, let t = self.tasks.tasks.first(where: { $0.actions.contains { $0.id == aid } }),
                   let a = t.actions.first(where: { $0.id == aid }), a.status != "approved" && a.status != "pending" {
                    self.model.taskChip = nil; self.chipAction = nil; self.chipTimeout?.cancel()
                }
            }
        }.store(in: &bag)
        // Finished / waiting tasks drop down from the island (only while it isn't in a conversation,
        // which reports them itself).
        tasks.onChange = { [weak self] t in
            guard let self else { return }
            if self.announce(t) { return }
            guard self.toasts, !self.overview.isOpen else { return }
            self.showToast(t)
        }
        tasks.start()
        if Hotkeys.fnAvailable { keys.enableFn() }
    }

    private func wire() {
        notch.onHover = { [weak self] inside in self?.hover(inside) }
        notch.onClose = { [weak self] in self?.close() }
        notch.onOpen = { [weak self] path in
            // A result card: its task in the full-screen overview (report, steps, files).
            if path.hasPrefix("task:") {
                Task { @MainActor in self?.close(); self?.overview.show(task: String(path.dropFirst(5))) }
                return
            }
            if path.hasPrefix("http"), let u = URL(string: path) { NSWorkspace.shared.open(u) } else { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }
        }
        notch.onConfirm = { [weak self] id, decision, args in self?.confirm(id, decision, args) }
        keys.onPTT = { [weak self] down in self?.ptt(down) }
        keys.onPTTCancel = { [weak self] in self?.pttCancel() }
        // fn only acts while the island is open (fn is also the volume/brightness modifier).
        keys.fnArmed = { [weak self] in
            guard let s = self?.model.state else { return false }
            return s != .hidden && s != .toast && s != .dictating
        }
        keys.onToggle = { [weak self] in self?.toggle() }
        keys.onEscape = { [weak self] in self?.close() }
        keys.onDictate = { [weak self] down in self?.dictate(down) }
        // Switching the character in the menu applies from the next sentence on.
        // Voice off/on (tray icon, island menu): applies at once, also mid-conversation.
        model.$voiceOff.removeDuplicates().sink { [weak self] off in
            Task { @MainActor in
                guard let self else { return }
                self.statusBar.dmodel.voiceOff = off
                self.audio.setMuted(off)
                if off, [.listening, .speaking, .thinking].contains(self.model.state) { self.close() }
            }
        }.store(in: &bag)
        model.$voiceChoice.dropFirst().removeDuplicates().sink { [weak self] _ in
            Task { @MainActor in
                guard let self, self.socket.isOpen else { return }
                self.socket.send(json: self.model.characterMessage)
            }
        }.store(in: &bag)

        audio.onFrame = { [weak self] f in
            guard let self else { return }
            self.socket.send(audio: f)
            Task { @MainActor in self.lastFrameSent = Date() }
        }
        audio.onLevel = { [weak self] lvl, bands in
            Task { @MainActor in
                guard let self else { return }
                self.model.level = lvl
                if !bands.isEmpty { self.model.bands = bands }
            }
        }
        audio.onPlaybackIdle = { [weak self] in Task { @MainActor in self?.playbackIdle() } }
        socket.onAudio = { [weak self] d in
            Task { @MainActor in
                guard let self else { return }
                if !self.replyStarted { self.replyStarted = true; self.audio.beginReply(); self.model.mark("first_audio") }
                // Sam starts talking while the island is closed (a result read out after the
                // user closed it): open it, hands-free, so the follow-up question is heard and
                // the user's answer is caught, instead of a voice from nowhere.
                if self.model.state == .hidden || self.model.state == .peek || self.model.state == .toast {
                    if self.model.state == .toast { self.dismissToast() }
                    self.open(.result, handsFree: true)
                }
                if self.model.state == .thinking || self.model.state == .result || self.model.state == .listening && !self.model.pttHeld { self.model.set(.speaking) }
                self.audio.play(d)
            }
        }
        socket.onJSON = { [weak self] j in Task { @MainActor in self?.event(j) } }
        socket.onState = { [weak self] up in Task { @MainActor in self?.model.connected = up; if up { self?.model.mark("ws_open") } } }
    }

    // MARK: activation
    func hover(_ inside: Bool) {
        hoverTask?.cancel()
        if model.state == .toast { model.toastHover = inside; return }
        if inside {
            collapseTask?.cancel()
            guard model.state == .hidden else { return }
            hoverTask = Task { [weak self] in
                try? await Task.sleep(nanoseconds: 200_000_000)       // dwell
                guard !Task.isCancelled, let self else { return }
                self.open(.peek)
            }
        } else if model.state == .peek {
            hoverTask = Task { [weak self] in
                try? await Task.sleep(nanoseconds: 120_000_000)       // exit grace
                guard !Task.isCancelled, let self, self.model.state == .peek else { return }
                self.close()
            }
        } else if model.state == .result && !model.hasConfirm {
            scheduleCollapse()
        }
    }

    /// Opens the island: connect (= warm the models), capture context, start the mic.
    /// Test hook: point at this AX coordinate instead of the real cursor.
    var testPoint: CGPoint?

    func open(_ to: IslandState, handsFree: Bool = true) {
        disconnectTask?.cancel(); collapseTask?.cancel()
        // The window under the cursor wins over the frontmost app (point and ask); selection
        // and URL still come from the focused app.
        // Test mode never reads the user's own frontmost window; tests point at their own windows.
        // Focus and ask is opt-in; the test hook stands for a user who switched it on.
        var ctx = testMode ? [:] : ContextCapture.capture()
        let readPointed = testMode ? testPoint != nil : ContextCapture.focusAsk
        let pointed = readPointed ? ContextCapture.pointed(at: testPoint) : [:]
        if pointed["text"] != nil { ctx.merge(pointed) { _, new in new } }
        model.contextChip = ContextCapture.chip(ctx)
        socket.connect()
        sendWhenOpen(model.characterMessage)
        if !ctx.isEmpty { sendWhenOpen(["type": "context"].merging(ctx) { a, _ in a }) }
        model.handsFree = handsFree
        model.error = nil
        if model.state == .hidden || model.state == .result { model.transcript = ""; if to != .peek { model.answer = "" } }
        model.set(to)
        // Test mode ignores the user's real Esc presses and clicks: they keep working on the Mac
        // while the E2E suite runs, and their input must not close the island mid-test.
        if !testMode { keys.escapeActive(true); installClickOutside() }
        // Voice off: no mic, silent playback; the island still shows cards and text.
        do { try audio.start(micOff: testMode || model.voiceOff) } catch { model.error = "Microphone unavailable: \(error.localizedDescription)" }
        audio.setMuted(model.voiceOff)
        model.followUp = false
        if handsFree && to == .listening { armIdle(idleFirst) } else { idleTask?.cancel() }
    }

    /// Closes hands-free listening if no speech starts within `seconds`.
    func armIdle(_ seconds: TimeInterval) {
        idleTask?.cancel()
        idleTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled, let self, self.model.handsFree, self.model.state == .listening, !self.model.pttHeld else { return }
            self.model.mark("handsfree_idle_close")
            self.close()
        }
    }

    private var toastTask: Task<Void, Never>?

    /// A request the user made by voice finished while the island was closed (its session
    /// dropped a minute after closing): reconnect, and the server reads the result out with a
    /// follow-up question (its result card opens the island). Not during a call or other
    /// recording (the mic is busy elsewhere) and not for monitors / automations / lead lists,
    /// which drop down as toasts. Returns false when the toast should show instead.
    private func announce(_ t: TaskItem) -> Bool {
        // Finished work and drafts waiting for an OK: both are worth reopening the island for.
        guard !testMode || announceInTests, !model.voiceOff, ["done", "failed", "needs_action"].contains(t.status), !["automation", "monitor", "leads"].contains(t.kind) else { return false }
        guard model.state == .hidden || model.state == .toast || model.state == .peek else { return false }
        if socket.isOpen { return true }   // the open session reports it itself
        guard !AudioIO.inputBusyElsewhere() else { return false }
        disconnectTask?.cancel()
        socket.connect()
        sendWhenOpen(model.characterMessage)
        model.mark("announce_\(t.id)")
        // Nothing to report after all (already heard in a conversation): let the models unload.
        disconnectTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            guard !Task.isCancelled, let self, self.model.state == .hidden || self.model.state == .toast else { return }
            self.socket.disconnect()
        }
        return true
    }

    func showToast(_ t: TaskItem) {
        guard model.state == .hidden || model.state == .peek || model.state == .toast else { return }
        model.toast = t
        model.set(.toast)
        toastTask?.cancel()
        toastTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 6_000_000_000)
            while let s = self, s.model.toastHover { try? await Task.sleep(nanoseconds: 400_000_000) }
            guard !Task.isCancelled, let self, self.model.state == .toast else { return }
            self.dismissToast()
        }
    }

    func dismissToast() {
        toastTask?.cancel()
        guard model.state == .toast else { return }
        model.toast = nil
        model.toastHover = false
        model.set(.hidden)
    }

    func toggle() {
        if model.state == .hidden || model.state == .peek || model.state == .toast { dismissToast(); open(.listening) } else { close() }
    }

    // MARK: dictation
    private var dictationTarget: DictationTarget?
    /// Tests dictate into their own app (never the user's frontmost one).
    var testDictationBundle: String?

    func dictate(_ down: Bool) {
        if down {
            guard model.state != .dictating else { return }
            if model.state == .toast { dismissToast() }
            disconnectTask?.cancel(); collapseTask?.cancel()
            audio.stopPlayback()
            dictationTarget = DictationTarget.capture(bundle: testMode ? testDictationBundle : nil)
            model.dictationTarget = dictationTarget?.app
            model.dictationResult = nil
            model.set(.dictating)
            socket.connect()
            sendWhenOpen(["type": "dictate", "state": "start"])
            do { try audio.start(micOff: testMode) } catch { model.error = "Microphone unavailable: \(error.localizedDescription)" }
            model.mark("dictate_start")
        } else {
            guard model.state == .dictating else { return }
            socket.send(json: ["type": "dictate", "state": "stop"])
            model.dictationResult = "Transcribing…"
            model.mark("dictate_stop")
        }
    }

    private func dictationDone(_ text: String, error: String?) {
        let outcome = text.isEmpty ? nil : dictationTarget?.insert(text) ?? DictationTarget(pid: 0, app: "", element: nil).insert(text)
        model.dictationResult = error.map { "Couldn't transcribe: \($0)" }
            ?? (text.isEmpty ? "Didn't catch anything" : (outcome == .copied ? "Copied — press ⌘V to paste" : "Inserted"))
        model.mark("dictation_\(outcome.map { "\($0)" } ?? "empty")")
        audio.stop()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { [weak self] in
            guard let self, self.model.state == .dictating else { return }
            self.close()
        }
    }

    func ptt(_ down: Bool) {
        if down {
            if model.state == .toast { dismissToast() }
            if model.state == .hidden || model.state == .peek || model.state == .result { open(.listening, handsFree: false) }
            audio.stopPlayback()
            model.pttHeld = true
            model.set(.listening)
            sendWhenOpen(["type": "ptt", "state": "down"])
        } else {
            guard model.pttHeld else { return }
            model.pttHeld = false
            socket.send(json: ["type": "ptt", "state": "up"])
            model.set(.thinking)
        }
    }

    /// fn hold turned into a key combo: no turn; back to the peek the user started from.
    func pttCancel() {
        guard model.pttHeld else { return }
        model.pttHeld = false
        socket.send(json: ["type": "ptt", "state": "cancel"])
        model.mark("ptt_cancel")
        close()
    }

    func close() {
        hoverTask?.cancel(); collapseTask?.cancel(); idleTask?.cancel()
        model.followUp = false
        audio.stop()
        socket.send(json: ["type": "playback", "state": "done"])
        model.pttHeld = false
        model.level = 0
        model.set(.hidden)
        notch.panel.allowKey = false
        keys.escapeActive(false)
        if let m = clickMonitor { NSEvent.removeMonitor(m); clickMonitor = nil }
        // Keep the session briefly for follow-ups; then disconnect so the models can unload.
        disconnectTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: (self?.testMode == true ? 5 : 60) * 1_000_000_000)
            // A toast may have dropped meanwhile: still no conversation, so let the models unload.
            guard !Task.isCancelled, let self, self.model.state == .hidden || self.model.state == .toast else { return }
            self.socket.disconnect()
            self.model.cards.removeAll { $0.kind != "confirm" }
        }
    }

    private func scheduleCollapse() {
        collapseTask?.cancel()
        collapseTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 8_000_000_000)
            guard !Task.isCancelled, let self, self.model.state == .result, !self.model.hasConfirm else { return }
            self.close()
        }
    }

    private func installClickOutside() {
        guard clickMonitor == nil else { return }
        clickMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown]) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.model.expanded, !self.model.hasConfirm else { return }
                if !self.notch.islandScreenRect().contains(NSEvent.mouseLocation) { self.close() }
            }
        }
    }

    private func sendWhenOpen(_ j: [String: Any], tries: Int = 40) {
        if socket.isOpen { socket.send(json: j); return }
        guard tries > 0 else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak self] in self?.sendWhenOpen(j, tries: tries - 1) }
    }

    // MARK: server events
    private func event(_ j: [String: Any]) {
        switch j["type"] as? String {
        case "user_speaking":
            idleTask?.cancel(); model.followUp = false
            audio.stopPlayback()        // barge-in
            replyStarted = false; agentDone = false
            model.answer = ""
            if model.state != .listening { model.set(.listening) }
            model.mark("user_speaking")
        case "transcript":
            model.transcript = j["text"] as? String ?? ""
            replyStarted = false; agentDone = false
            model.answer = ""
            if !model.pttHeld { model.set(.thinking) }
            model.mark("transcript")
        case "agent_text":
            // After a finished reply, new agent text is a new reply the agent started itself
            // (it reads out a background result): fresh answer, fresh first-audio mark.
            if agentDone { agentDone = false; replyStarted = false; model.answer = "" }
            let t = (j["text"] as? String ?? "").trimmingCharacters(in: .whitespaces)
            model.answer = model.answer.isEmpty ? t : model.answer + " " + t
        case "interrupt":
            audio.stopPlayback()
        case "agent_done":
            agentDone = true
            model.mark("agent_done")
            if !audio.isPlaying { playbackIdle() }
        case "ptt_empty":
            // Held and released, but no speech reached the server: say so instead of nothing.
            let peak = j["peak_db"] as? Double ?? 0
            model.error = (j["secs"] as? Double ?? 0) < 0.4 ? nil : (peak < -45 ? "Didn't hear anything — check the microphone (System Settings › Sound › Input)" : "Didn't catch that — try again")
            model.set(model.handsFree ? .listening : .peek)
            if model.handsFree { armIdle(idleFollowUp) }
        case "card":
            guard let c = Card(j) else { return }
            model.cards.removeAll { $0.id == c.id }
            model.cards.insert(c, at: 0)
            if model.cards.count > 3 { model.cards.removeLast(model.cards.count - 3) }
            if model.state == .toast { dismissToast() }
            if model.state == .hidden || model.state == .peek {
                if c.kind == "result" {
                    // A finished request is read out with a follow-up question: hands-free, so
                    // the island listens for the answer afterwards (and closes if none comes).
                    open(.result, handsFree: true)
                } else {
                    // Confirmations and errors: playback on, mic off.
                    open(.result, handsFree: false); audio.stop(); try? audio.start(micOff: true)
                }
            }
            // Never take the keyboard on our own: the user keeps typing in their app while a
            // confirmation waits. The panel becomes key only after a click into it.
            if c.kind == "confirm" { keys.escapeActive(false) }
            model.mark("card_\(c.kind)")
        case "dictation":
            dictationDone(j["text"] as? String ?? "", error: j["error"] as? String)
        case "error":
            model.error = j["message"] as? String
        default: break
        }
    }

    private func playbackIdle() {
        // The server only estimates when playback ends; the real moment decides when the next
        // queued result may be read out (a result used to cut the previous one off).
        socket.send(json: ["type": "playback", "state": "done"])
        guard agentDone, model.state == .speaking || model.state == .thinking else { return }
        model.mark("playback_done")
        if model.handsFree {
            // Sam asked something (a game move, a choice): give the answer more room than a
            // reply that just ended (2026-10-13: a twenty-questions game closed after one "no").
            let asked = model.answer.trimmingCharacters(in: .whitespacesAndNewlines).hasSuffix("?")
            model.set(.listening); model.followUp = true; armIdle(asked ? idleFollowUp * 2 : idleFollowUp)
        } else {
            model.set(.result); keys.escapeActive(false); scheduleCollapse()
            // Reply over and not hands-free: mic (and its voice processing) off, so other audio
            // isn't lowered while a result or an approval card stays on screen.
            if !testMode { audio.stop() }
        }
    }

    private func confirm(_ id: String, _ decision: String, _ args: [String: Any]) {
        var j: [String: Any] = ["type": "confirm", "action_id": id, "decision": decision]
        if !args.isEmpty { j["args"] = args }
        socket.connect()
        sendWhenOpen(j)
        model.cards.removeAll { ($0.confirm?["action_id"] as? String) == id }
        notch.panel.allowKey = false
        model.taskChip = decision == "approve" ? "Working on it…" : nil
        chipAction = decision == "approve" ? id : nil
        chipTimeout?.cancel()
        if decision == "approve" {
            // Safety net: the badge never outlives the work (3 min), even if a poll is missed.
            chipTimeout = Task { [weak self] in
                try? await Task.sleep(for: .seconds(180))
                guard !Task.isCancelled else { return }
                self?.model.taskChip = nil; self?.chipAction = nil
            }
        }
        model.mark("confirm_\(decision)")
    }

    // MARK: test hooks (TestAPI)
    func injectWav(_ path: String, done: @escaping () -> Void) {
        audio.inject(wav: URL(fileURLWithPath: path), then: done)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var controller: Controller!
    var api: TestAPI?
    func applicationWillTerminate(_ n: Notification) {
        MainActor.assumeIsolated { Services.shared.stopAll() }
    }
    func applicationDidFinishLaunching(_ n: Notification) {
        NSApp.setActivationPolicy(.accessory)
        installEditMenu()
        MainActor.assumeIsolated {
            controller = Controller()
            if controller.testMode { api = TestAPI(controller: controller) }
            // Sam.app runs its own services (server, voice, memory, helper); tests bring theirs.
            if !controller.testMode {
                Services.shared.start()
                // First run: the one-time download starts right away (onboarding shows its progress).
                if Services.shared.bundled && !Services.shared.runtimeReady { Services.shared.install() }
                // App updates (Sparkle) and the model manifest.
                if Services.shared.bundled { Updates.shared.start() }
            }
            // First run: the onboarding explains and asks for each permission (mic included).
            if !controller.testMode && !OnboardingController.done {
                controller.onboarding.show()
            } else if AudioIO.micPermission() == .notDetermined && !controller.testMode {
                AudioIO.requestMic { _ in }
            }
        }
    }
}

/// A menu-bar-only app has no visible menus, but text fields still need the Edit menu's key
/// equivalents: without it ⌘V / ⌘C / ⌘X / ⌘A / ⌘Z do nothing in Sam's windows.
func installEditMenu() {
    let main = NSMenu()
    let editItem = NSMenuItem()
    main.addItem(editItem)
    let edit = NSMenu(title: "Edit")
    edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
    let redo = edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
    redo.keyEquivalentModifierMask = [.command, .shift]
    edit.addItem(.separator())
    edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
    edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
    edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
    edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
    editItem.submenu = edit
    NSApp.mainMenu = main
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
