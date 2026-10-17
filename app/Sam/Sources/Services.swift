// Sam's background services, owned by the app: the agent server, the speech and memory sidecars
// (Python + MLX in a private runtime) and the computer-use helper. Nothing to start by hand:
// the app installs the runtime on first run (bundled uv, pinned packages, models downloaded
// once into ~/Library/Application Support/Sam), starts everything at launch, restarts what
// crashes and stops it all on quit. Logs: ~/Library/Logs/Sam/.
import AppKit
import CryptoKit
import Foundation

@MainActor
final class Services: ObservableObject {
    enum Setup: Equatable { case unknown, needed, running(String), ready, failed(String) }
    @Published private(set) var setup: Setup = .unknown
    @Published private(set) var progress: Double = 0
    @Published private(set) var serverUp = false

    static let shared = Services()
    let support = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/Sam")
    let logs = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Sam")
    var resources: URL { Bundle.main.resourceURL! }
    var runtime: URL { support.appendingPathComponent("runtime") }
    var python: URL { runtime.appendingPathComponent("venv/bin/python") }
    var hfHome: URL { support.appendingPathComponent("models") }
    var dataDir: URL { support.appendingPathComponent("data") }
    nonisolated static let dataDirURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/Sam/data")
    /// The runtime's identity: a hash of the bundled package list. An app update that changes
    /// requirements.lock no longer matches the installed runtime's `.ready`, so setup runs again.
    nonisolated static let runtimeVersion: String = {
        guard let u = Bundle.main.url(forResource: "requirements", withExtension: "lock", subdirectory: "runtime"),
              let d = try? Data(contentsOf: u) else { return "3" }
        return SHA256.hash(data: d).prefix(8).map { String(format: "%02x", $0) }.joined()
    }()
    // voice-glitch lab (2026-10-14): with short pieces synthesized whole and the retake guard the
    // 0.6B clone glitches on 3.8 % of pieces (UTMOS 4.05); the 1.7B on 0 % (4.19) for 0.5 GB more
    // and a slower first word. The user chooses in the onboarding ("light" | "steady", data/voice.json).
    // Model repos: the model manifest's (Updates.swift), else these defaults.
    nonisolated static var ttsRepo: String { Updates.repo("tts", "mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit") }
    nonisolated static var ttsRepoSteady: String { Updates.repo("tts_steady", "mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit") }
    /// Kyutai Pocket TTS: 100M parameters, ~0.4 GB in memory, real-time factor 0.1; plainer sound, English only.
    nonisolated static var ttsRepoTiny: String { Updates.repo("tts_tiny", "mlx-community/pocket-tts") }
    nonisolated static var embedRepo: String { Updates.repo("embed", "mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ") }
    /// Laya fine-tuned on Sam's micro-judgments, and the end-of-turn detector.
    nonisolated static var judgeRepo: String { Updates.repo("judge", "elijahross/laya-sam") }
    nonisolated static var turnRepo: String { Updates.repo("turn", "mlx-community/smart-turn-v3") }
    /// What the sidecar is started with: the saved choice, else the light model.
    var voiceRepo: String {
        let j = (try? JSONSerialization.jsonObject(with: Data(contentsOf: dataDir.appendingPathComponent("voice.json")))) as? [String: Any]
        switch j?["model"] as? String { case "steady": return Self.ttsRepoSteady; case "tiny": return Self.ttsRepoTiny; default: return Self.ttsRepo }
    }
    /// Progress text while a chosen voice model is fetched and the sidecar restarts (nil = idle).
    @Published private(set) var voiceModelStatus: String?

    private var procs: [String: Process] = [:]
    private var restarts: [String: [Date]] = [:]
    private var stopping = false
    /// A server already running on 47800 (development: ./run.sh): the app doesn't start its own.
    private(set) var external = false

    /// Inside Sam.app (bundled server + sidecars); a development build runs against ./run.sh.
    var bundled: Bool { FileManager.default.fileExists(atPath: resources.appendingPathComponent("bin/sam-server").path) }

    var runtimeReady: Bool {
        guard FileManager.default.isExecutableFile(atPath: python.path) else { return false }
        let marker = runtime.appendingPathComponent(".ready")
        let have = (try? String(contentsOf: marker, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines)
        // Runtimes installed before the hash marker ("3") hold the same package list.
        if have == "3" { try? Self.runtimeVersion.write(to: marker, atomically: true, encoding: .utf8); return true }
        return have == Self.runtimeVersion
    }

    // MARK: lifecycle

    func start() {
        guard bundled else { setup = .ready; return }
        try? FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        try? FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)
        Task {
            if await Self.listening(47800) {
                external = true
                serverUp = true
            } else {
                startServer()
            }
            if runtimeReady {
                setup = .ready
                startSidecars()
            } else if case .running = setup {
                // install() already started (first launch)
            } else {
                setup = .needed
            }
            startHelper()
        }
    }

    func stopAll() {
        stopping = true
        // The helper runs as its own app (launched, not a child process): quit it too.
        for a in NSRunningApplication.runningApplications(withBundleIdentifier: "one.sam.computeruse") { a.terminate() }
        for (_, p) in procs where p.isRunning { p.terminate() }
        let deadline = Date().addingTimeInterval(3)
        while procs.values.contains(where: \.isRunning) && Date() < deadline { usleep(100_000) }
        for (_, p) in procs where p.isRunning { kill(p.processIdentifier, SIGKILL) }
    }

    private func env() -> [String: String] {
        var e = ProcessInfo.processInfo.environment
        e["DATA_DIR"] = dataDir.path
        e["HF_HOME"] = hfHome.path
        e["SAM_RESOURCES"] = resources.appendingPathComponent("server").path
        e["WORKSPACE_BIN"] = resources.appendingPathComponent("bin/sam-workspace").path
        e["RUST_LOG"] = e["RUST_LOG"] ?? "info"
        e["PYTHONUNBUFFERED"] = "1"
        // The same port block as development.
        e["BIND"] = "127.0.0.1:47800"
        return e
    }

    private func launch(_ name: String, _ exe: URL, _ args: [String], cwd: URL? = nil) {
        let p = Process()
        p.executableURL = exe
        p.arguments = args
        p.environment = env()
        if let cwd { p.currentDirectoryURL = cwd }
        let log = logs.appendingPathComponent("\(name).log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        if let h = try? FileHandle(forWritingTo: log) { h.seekToEndOfFile(); p.standardOutput = h; p.standardError = h }
        p.terminationHandler = { [weak self] proc in
            Task { @MainActor in self?.exited(name, exe, args, cwd, status: proc.terminationStatus) }
        }
        do {
            try p.run()
            procs[name] = p
        } catch {
            NSLog("Sam: couldn't start \(name): \(error)")
        }
    }

    /// Crashed → restart with a pause; more than 5 crashes in 10 minutes → give up (logged).
    private func exited(_ name: String, _ exe: URL, _ args: [String], _ cwd: URL?, status: Int32) {
        guard !stopping else { return }
        let now = Date()
        restarts[name] = (restarts[name] ?? []).filter { now.timeIntervalSince($0) < 600 } + [now]
        if name == "server" { serverUp = false }
        guard (restarts[name]?.count ?? 0) <= 5 else { NSLog("Sam: \(name) keeps crashing (status \(status)); see ~/Library/Logs/Sam/\(name).log"); return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
            guard let self, !self.stopping else { return }
            self.launch(name, exe, args, cwd: cwd)
            if name == "server" { Task { await self.waitServer() } }
        }
    }

    private func startServer() {
        launch("server", resources.appendingPathComponent("bin/sam-server"), [], cwd: dataDir)
        Task { await waitServer() }
    }

    private func waitServer() async {
        for _ in 0..<120 {
            if await Self.listening(47800) { serverUp = true; return }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    /// The voice picked in the island menu: the model's load warm-up speaks in it, so a session's
    /// first reply doesn't meet a voice the model hasn't spoken yet (voice-prewarm lab). A switch
    /// while the sidecar runs is covered by the prewarm sent when the user starts talking.
    nonisolated static var savedVoice: String {
        let v = UserDefaults.standard.string(forKey: "character") ?? "sam-male"
        return ["sam-male", "sam-female"].contains(v) ? v : "sam-male"
    }

    private var speechArgs: [String] {
        let side = resources.appendingPathComponent("sidecar")
        return [side.appendingPathComponent("speech_server.py").path, "--tts", voiceRepo, "--stt", "apple", "--turn-model", Self.turnRepo,
                "--voices", resources.appendingPathComponent("voices").path, "--voice", Self.savedVoice]
    }

    /// The user picked another voice model: fetch it if needed (2.4 GB for the steady one), then
    /// restart the speech sidecar with it. Safe to call again; reports through `voiceModelStatus`.
    func applyVoiceModel() {
        guard runtimeReady, voiceModelStatus == nil else { return }
        let repo = voiceRepo
        voiceModelStatus = repo == Self.ttsRepoSteady ? "Fetching the steadier voice (2.4 GB)…" : repo == Self.ttsRepoTiny ? "Fetching the tiny voice (0.3 GB)…" : "Switching the voice…"
        Task.detached { [python = python, env = env()] in
            let p = Process()
            p.executableURL = python
            p.arguments = ["-c", "from huggingface_hub import snapshot_download as s; s('\(repo)')"]
            p.environment = env
            p.standardOutput = FileHandle.nullDevice; p.standardError = FileHandle.nullDevice
            let ok = (try? p.run()) != nil
            if ok { p.waitUntilExit() }
            await MainActor.run { [weak self] in
                guard let self else { return }
                if ok && p.terminationStatus == 0 {
                    self.restartSpeech()
                    self.voiceModelStatus = nil
                } else {
                    self.voiceModelStatus = "Couldn't fetch the voice model; check the connection and try again."
                    DispatchQueue.main.asyncAfter(deadline: .now() + 6) { [weak self] in if self?.voiceModelStatus?.hasPrefix("Couldn't") == true { self?.voiceModelStatus = nil } }
                }
            }
        }
    }

    /// Stops the speech sidecar without the crash-restart and starts it again with the current arguments.
    private func restartSpeech() {
        if let p = procs["speech"], p.isRunning {
            p.terminationHandler = nil
            p.terminate()
            let deadline = Date().addingTimeInterval(3)
            while p.isRunning && Date() < deadline { usleep(50_000) }
            if p.isRunning { kill(p.processIdentifier, SIGKILL) }
        }
        launch("speech", python, speechArgs, cwd: resources.appendingPathComponent("sidecar"))
    }

    private func startSidecars() {
        guard runtimeReady else { return }
        let side = resources.appendingPathComponent("sidecar")
        launch("speech", python, speechArgs, cwd: side)
        launch("mind", python, mindArgs, cwd: side)
    }

    private var mindArgs: [String] {
        [resources.appendingPathComponent("sidecar/mind_server.py").path, "--judge-checkpoint", Self.judgeRepo, "--embed-model", Self.embedRepo]
    }

    /// Stops a service without the crash-restart and starts it again with fresh arguments.
    private func relaunch(_ name: String, _ exe: URL, _ args: [String], cwd: URL? = nil) {
        if let p = procs[name], p.isRunning {
            p.terminationHandler = nil
            p.terminate()
            let deadline = Date().addingTimeInterval(3)
            while p.isRunning && Date() < deadline { usleep(50_000) }
            if p.isRunning { kill(p.processIdentifier, SIGKILL) }
        }
        launch(name, exe, args, cwd: cwd)
    }

    /// A new model manifest (Updates.swift): fetch any new local model, then restart the services
    /// so the server reads the new model ids and the sidecars load the new repos.
    func applyManifest(modelsChanged: Bool) {
        guard bundled, !external else { return }
        let restart = { [weak self] in
            guard let self else { return }
            if self.runtimeReady {
                let side = self.resources.appendingPathComponent("sidecar")
                self.relaunch("speech", self.python, self.speechArgs, cwd: side)
                self.relaunch("mind", self.python, self.mindArgs, cwd: side)
            }
            self.serverUp = false
            self.relaunch("server", self.resources.appendingPathComponent("bin/sam-server"), [], cwd: self.dataDir)
            Task { await self.waitServer() }
        }
        guard modelsChanged, runtimeReady else { restart(); return }
        let repos = [voiceRepo, Self.embedRepo, Self.judgeRepo, Self.turnRepo]
        Task.detached { [python = python, env = env()] in
            let p = Process()
            p.executableURL = python
            p.arguments = ["-c", "import sys\nfrom huggingface_hub import snapshot_download as s\nfor r in sys.argv[1:]: s(r)"] + repos
            p.environment = env
            p.standardOutput = FileHandle.nullDevice; p.standardError = FileHandle.nullDevice
            if (try? p.run()) != nil { p.waitUntilExit() }
            await MainActor.run { restart() }
        }
    }

    /// The computer-use helper inside the app (Contents/Library/Helpers): background, no Dock icon.
    func startHelper() {
        let helper = Bundle.main.bundleURL.appendingPathComponent("Contents/Library/Helpers/Sam Computer Use.app")
        guard FileManager.default.fileExists(atPath: helper.path) else { return }
        let c = NSWorkspace.OpenConfiguration()
        c.activates = false
        c.addsToRecentItems = false
        NSWorkspace.shared.openApplication(at: helper, configuration: c) { _, _ in }
    }

    var helperURL: URL? {
        let h = Bundle.main.bundleURL.appendingPathComponent("Contents/Library/Helpers/Sam Computer Use.app")
        return FileManager.default.fileExists(atPath: h.path) ? h : nil
    }

    // MARK: first-run setup

    /// Private runtime + models. Safe to call again (resumes where it stopped).
    func install() {
        if case .running = setup { return }
        guard bundled else { setup = .ready; return }
        setup = .running("Preparing…")
        progress = 0.02
        Task.detached { [support = support, runtime = runtime, python = python, hf = hfHome, res = resources] in
            func run(_ exe: String, _ args: [String], env extra: [String: String] = [:]) -> (Int32, String) {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: exe)
                p.arguments = args
                var e = ProcessInfo.processInfo.environment
                e["UV_PYTHON_INSTALL_DIR"] = runtime.appendingPathComponent("python").path
                e["UV_CACHE_DIR"] = support.appendingPathComponent("cache/uv").path
                e["HF_HOME"] = hf.path
                for (k, v) in extra { e[k] = v }
                p.environment = e
                let pipe = Pipe()
                p.standardOutput = pipe; p.standardError = pipe
                do { try p.run() } catch { return (-1, "\(error)") }
                let out = pipe.fileHandleForReading.readDataToEndOfFile()
                p.waitUntilExit()
                return (p.terminationStatus, String(decoding: out.suffix(2000), as: UTF8.self))
            }
            let uv = res.appendingPathComponent("bin/uv").path
            try? FileManager.default.createDirectory(at: runtime, withIntermediateDirectories: true)
            let steps: [(String, Double, () -> (Int32, String))] = [
                ("Getting Python…", 0.1, { run(uv, ["venv", "--python", "3.12", "--python-preference", "only-managed", "--clear", runtime.appendingPathComponent("venv").path]) }),
                ("Installing the voice and memory engines (about 1.5 GB)…", 0.55, {
                    run(uv, ["pip", "install", "--python", python.path, "-r", res.appendingPathComponent("runtime/requirements.lock").path]) }),
                // Pulled in by dependencies but never loaded by Sam (measured): about 260 MB.
                ("Tidying up…", 0.6, { _ = run(uv, ["pip", "uninstall", "--python", python.path, "opencv-python", "opencv-python-headless", "numba", "llvmlite"]); return (0, "") }),
                ("Downloading Sam's voice model…", 0.8, { run(python.path, ["-c", "from huggingface_hub import snapshot_download as s; s('\(Services.ttsRepo)')"]) }),
                ("Downloading the memory model…", 0.9, { run(python.path, ["-c", "from huggingface_hub import snapshot_download as s; s('\(Services.embedRepo)')"]) }),
                ("Downloading Sam's judgment and turn-taking models…", 0.97, { run(python.path, ["-c", "from huggingface_hub import snapshot_download as s; s('\(Services.judgeRepo)'); s('\(Services.turnRepo)')"]) }),
            ]
            for (label, p, step) in steps {
                await MainActor.run { Services.shared.setup = .running(label) }
                let (code, out) = step()
                if code != 0 {
                    let tail = out.split(separator: "\n").suffix(3).joined(separator: " ")
                    await MainActor.run { Services.shared.setup = .failed("\(label) failed: \(tail)") }
                    return
                }
                await MainActor.run { Services.shared.progress = p }
            }
            try? Services.runtimeVersion.write(to: runtime.appendingPathComponent(".ready"), atomically: true, encoding: .utf8)
            // The package download cache is only needed while installing (about 1.4 GB).
            try? FileManager.default.removeItem(at: support.appendingPathComponent("cache"))
            await MainActor.run {
                let s = Services.shared
                s.progress = 1
                s.setup = .ready
                s.startSidecars()
                // The server picks up memory and judgments from the sidecars on its next start.
                if !s.external, let p = s.procs["server"], p.isRunning { p.terminate() }
            }
        }
    }

    // MARK: helpers

    static func listening(_ port: UInt16) async -> Bool {
        await withCheckedContinuation { (c: CheckedContinuation<Bool, Never>) in
            DispatchQueue.global().async {
                let fd = socket(AF_INET, SOCK_STREAM, 0)
                defer { close(fd) }
                var a = sockaddr_in()
                a.sin_family = sa_family_t(AF_INET)
                a.sin_port = port.bigEndian
                a.sin_addr.s_addr = inet_addr("127.0.0.1")
                let r = withUnsafePointer(to: &a) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) } }
                c.resume(returning: r == 0)
            }
        }
    }
}
