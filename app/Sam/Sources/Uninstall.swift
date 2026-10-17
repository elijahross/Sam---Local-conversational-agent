// Uninstall Sam: everything it put on this Mac goes, not just the bundle. Trashing the app by
// hand leaves about 4.7 GB (models, runtime, data), logs, caches, preferences, Keychain items
// and the login item behind. The confirmation lists what will be removed; test mode never runs it.
import AppKit
import ServiceManagement

enum Uninstaller {
    struct Item { let path: URL; let what: String }

    static let home = FileManager.default.homeDirectoryForCurrentUser
    static let keychainServices = ["sam.browser", "sam.browser.jarkey", "sam.email", "sam.mcp", "sam.llm", "sam.spotify"]

    /// Folders and files Sam created outside its bundle (only those that exist).
    static func items() -> [Item] {
        [Item(path: home.appendingPathComponent("Library/Application Support/Sam"), what: "models, runtime, data (memory, history, browser sessions, reports)"),
         Item(path: home.appendingPathComponent("Library/Logs/Sam"), what: "logs"),
         Item(path: home.appendingPathComponent("Library/Caches/one.sam.app"), what: "caches"),
         Item(path: home.appendingPathComponent("Library/HTTPStorages/one.sam.app"), what: "network storage"),
         Item(path: home.appendingPathComponent("Library/Preferences/one.sam.app.plist"), what: "preferences")]
            .filter { FileManager.default.fileExists(atPath: $0.path.path) }
    }

    static func size(_ url: URL) -> Int64 {
        guard let e = FileManager.default.enumerator(at: url, includingPropertiesForKeys: [.fileSizeKey, .isRegularFileKey]) else {
            return Int64((try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
        }
        var n: Int64 = 0
        for case let f as URL in e {
            if let v = try? f.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey]), v.isRegularFile == true { n += Int64(v.fileSize ?? 0) }
        }
        return n
    }

    /// What an uninstall would remove, for the dialog and the tests (never acts).
    static func plan() -> [[String: Any]] {
        items().map { ["path": $0.path.path, "what": $0.what, "bytes": size($0.path)] }
            + [["path": "Keychain: " + keychainServices.joined(separator: ", "), "what": "saved logins, mail password, MCP and Spotify keys, cookie-jar key", "bytes": 0],
               ["path": "Login item", "what": "Open Sam when I log in", "bytes": 0],
               ["path": Bundle.main.bundleURL.path, "what": "the app (moved to the Trash)", "bytes": size(Bundle.main.bundleURL)]]
    }

    @MainActor
    static func confirmAndRun() {
        guard !CommandLine.arguments.contains("--test"), ProcessInfo.processInfo.environment["SAM_TEST"] != "1" else { return }
        let plan = plan()
        let total = plan.reduce(Int64(0)) { $0 + (($1["bytes"] as? Int64) ?? 0) }
        let fmt = ByteCountFormatter(); fmt.countStyle = .file
        let a = NSAlert()
        a.messageText = "Uninstall Sam?"
        a.informativeText = "This removes Sam and everything it keeps on this Mac (\(fmt.string(fromByteCount: total))):\n\n"
            + plan.map { "• \($0["what"] ?? "")" }.joined(separator: "\n")
            + "\n\nPrivacy permissions (Accessibility, Screen Recording, Calendar, Microphone…) are reset for Sam as far as macOS allows; the rest can be cleared in System Settings › Privacy & Security."
        a.alertStyle = .warning
        a.addButton(withTitle: "Uninstall")
        a.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn else { return }
        run()
    }

    @MainActor
    static func run() {
        Services.shared.stopAll()
        try? SMAppService.mainApp.unregister()
        // Keychain: every item of Sam's services (one call deletes one item; repeat until none).
        // Items saved before the rename ("rowan.*") that were never read again go too.
        for svc in keychainServices + keychainServices.map({ "rowan." + $0.dropFirst(4) }) {
            for _ in 0..<50 {
                let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/security")
                p.arguments = ["delete-generic-password", "-s", svc]
                p.standardOutput = nil; p.standardError = nil
                guard (try? p.run()) != nil else { break }
                p.waitUntilExit()
                if p.terminationStatus != 0 { break }
            }
        }
        for bid in ["one.sam.app", "one.sam.computeruse"] {
            let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
            p.arguments = ["reset", "All", bid]
            p.standardOutput = nil; p.standardError = nil
            try? p.run(); p.waitUntilExit()
        }
        UserDefaults.standard.removePersistentDomain(forName: Bundle.main.bundleIdentifier ?? "one.sam.app")
        for it in items() { try? FileManager.default.removeItem(at: it.path) }
        try? FileManager.default.trashItem(at: Bundle.main.bundleURL, resultingItemURL: nil)
        let done = NSAlert()
        done.messageText = "Sam was removed"
        done.informativeText = "The app is in the Trash; its models, runtime, data, logs, caches, preferences, Keychain items and login item are gone. Anything left under Privacy & Security can be cleared there."
        done.addButton(withTitle: "OK")
        done.runModal()
        NSApp.terminate(nil)
    }
}
