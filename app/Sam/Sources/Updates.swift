// Updates, in two layers, both signed with one Ed25519 key (Sparkle's EdDSA key; public half in
// Info.plist SUPublicEDKey, private half in the release maker's Keychain):
//  - Sam.app itself: Sparkle reads appcast.xml from the latest GitHub release (SUFeedURL), checks
//    daily, and verifies every download before installing it.
//  - The model manifest (release/manifest.json): model ids and Hugging Face repos that change
//    without a new app (a provider retires a model). Checked at launch and daily; a copy is
//    accepted only with a valid signature, a higher version and a build this app satisfies. The
//    verified copy goes to data/manifest.json, where the server reads it; the app downloads any
//    new local model, then restarts the services with it.
import AppKit
import CryptoKit
import Foundation
import Sparkle

struct ModelManifest: Codable {
    var version: Int
    var min_app_build: Int?
    var hf: [String: String]?
    var message: String?
}

@MainActor
final class Updates: ObservableObject {
    static let shared = Updates()
    /// A note from the latest manifest (e.g. "MiniMax M2 retires on 18 Nov; Sam has moved on").
    @Published private(set) var message: String?

    private var sparkle: SPUStandardUpdaterController?
    private var timer: Timer?

    var appBuild: Int { Int(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "") ?? 0 }
    private var cached: URL { Services.shared.dataDir.appendingPathComponent("manifest.json") }
    private var bundledManifest: URL? { Bundle.main.url(forResource: "manifest", withExtension: "json") }

    /// Starts both layers. Test mode and development builds (no feed) skip it.
    func start() {
        guard Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") != nil else { return }
        sparkle = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
        adoptBundled()
        message = current?.message
        Task { await checkManifest() }
        timer = Timer.scheduledTimer(withTimeInterval: 24 * 3600, repeats: true) { _ in
            Task { @MainActor in await Updates.shared.checkManifest() }
        }
    }

    var canCheck: Bool { sparkle != nil }
    func checkForUpdates() { sparkle?.checkForUpdates(nil) }

    // MARK: model manifest

    /// The manifest in effect: the cached one (verified when it was saved) or the app's own.
    var current: ModelManifest? {
        [cached, bundledManifest].compactMap { $0 }.compactMap { (try? Data(contentsOf: $0)).flatMap { try? JSONDecoder().decode(ModelManifest.self, from: $0) } }
            .filter { ($0.min_app_build ?? 0) <= appBuild }.max { $0.version < $1.version }
    }

    /// A Hugging Face repo from the manifest, else the compiled default.
    nonisolated static func repo(_ key: String, _ fallback: String) -> String {
        let url = Services.dataDirURL.appendingPathComponent("manifest.json")
        let base = Bundle.main.url(forResource: "manifest", withExtension: "json")
        let all = [url, base].compactMap { $0 }.compactMap { (try? Data(contentsOf: $0)).flatMap { try? JSONDecoder().decode(ModelManifest.self, from: $0) } }
        let build = Int(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "") ?? 0
        let best = all.filter { ($0.min_app_build ?? 0) <= build }.max { $0.version < $1.version }
        guard let r = best?.hf?[key], r.range(of: #"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$"#, options: .regularExpression) != nil else { return fallback }
        return r
    }

    /// A new app version ships a newer manifest than the cached one: the server should see it too.
    private func adoptBundled() {
        guard let b = bundledManifest, let data = try? Data(contentsOf: b),
              let m = try? JSONDecoder().decode(ModelManifest.self, from: data) else { return }
        let have = (try? Data(contentsOf: cached)).flatMap { try? JSONDecoder().decode(ModelManifest.self, from: $0) }
        if (have?.version ?? 0) < m.version { try? data.write(to: cached, options: .atomic) }
    }

    private var publicKey: Curve25519.Signing.PublicKey? {
        guard let b64 = Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String, let raw = Data(base64Encoded: b64) else { return nil }
        return try? Curve25519.Signing.PublicKey(rawRepresentation: raw)
    }

    /// Fetches manifest.json + manifest.json.sig; applies a newer, correctly signed manifest.
    func checkManifest() async {
        guard let s = Bundle.main.object(forInfoDictionaryKey: "SamManifestURL") as? String, let url = URL(string: s),
              let sigURL = URL(string: s + ".sig"), let key = publicKey else { return }
        func get(_ u: URL) async -> Data? {
            var r = URLRequest(url: u, timeoutInterval: 20)
            r.cachePolicy = .reloadIgnoringLocalCacheData
            guard let (d, resp) = try? await URLSession.shared.data(for: r), (resp as? HTTPURLResponse)?.statusCode == 200 else { return nil }
            return d
        }
        guard let data = await get(url), let sigText = await get(sigURL),
              let sig = Data(base64Encoded: String(decoding: sigText, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)),
              key.isValidSignature(sig, for: data) else { return }
        guard let m = try? JSONDecoder().decode(ModelManifest.self, from: data), m.version > (current?.version ?? 0) else { return }
        guard (m.min_app_build ?? 0) <= appBuild else {
            message = "A Sam update is needed for the latest model changes."
            return
        }
        let before = current?.hf ?? [:]
        try? data.write(to: cached, options: .atomic)
        message = m.message
        NSLog("Sam: model manifest \(m.version) applied")
        Services.shared.applyManifest(modelsChanged: (m.hf ?? [:]) != before)
    }
}
