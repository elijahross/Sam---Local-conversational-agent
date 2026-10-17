// What the user is looking at (X1): frontmost app, window title, selected text, browser URL —
// read through Accessibility at activation time only. Without AX permission: app name only.
// Both kinds are opt-in (onboarding › Connectors, island menu): nothing is read until the user
// switches one on, because whatever is on screen (an .env file, a key) goes to the AI provider.
import AppKit
import ApplicationServices

enum ContextCapture {
    /// "Highlight and ask": the selected text of the focused app.
    static var highlightAsk: Bool {
        get { UserDefaults.standard.bool(forKey: "ctxHighlight") }
        set { UserDefaults.standard.set(newValue, forKey: "ctxHighlight") }
    }
    /// "Focus and ask": the window title, URL, file and text of the window in front / under the pointer.
    static var focusAsk: Bool {
        get { UserDefaults.standard.bool(forKey: "ctxFocus") }
        set { UserDefaults.standard.set(newValue, forKey: "ctxFocus") }
    }
    static let leakWarning = "Whatever is on screen goes to your AI provider with your question. With an .env file, a password or an API key in view, ask Sam nothing about it, or it may leave your Mac. Password managers are always skipped."

    static var trusted: Bool { AXIsProcessTrusted() }

    private static func attr(_ e: AXUIElement, _ a: String) -> AnyObject? {
        var v: AnyObject?
        return AXUIElementCopyAttributeValue(e, a as CFString, &v) == .success ? v : nil
    }

    /// Password managers and credential stores (same list as the Computer Use helper).
    static func sensitive(_ bundle: String) -> Bool {
        let b = bundle.lowercased()
        return ["com.apple.keychainaccess", "com.apple.passwords", "com.agilebits.onepassword", "com.1password.", "com.bitwarden.", "com.lastpass.", "com.dashlane.",
                "org.keepassxc.", "com.enpass.", "in.sinew.enpass", "com.nordpass.", "com.protonpass.", "me.proton.pass"].contains { b == $0 || b.hasPrefix($0) }
    }

    static func capture() -> [String: String] {
        let highlight = highlightAsk, focus = focusAsk
        guard highlight || focus else { return [:] }
        // System surfaces aren't "what the user is looking at" (lock screen, alerts, Dock…).
        let system: Set<String> = ["com.apple.loginwindow", "com.apple.UserNotificationCenter", "com.apple.dock", "com.apple.systemuiserver",
                                   "com.apple.controlcenter", "com.apple.WindowManager", "com.apple.notificationcenterui", "com.apple.Spotlight"]
        guard let app = NSWorkspace.shared.frontmostApplication, let bid = app.bundleIdentifier,
              bid != Bundle.main.bundleIdentifier, !system.contains(bid), !bid.hasPrefix("one.sam.") else { return [:] }
        var out = ["app": app.localizedName ?? app.bundleIdentifier ?? "?"]
        // Password managers: the app's name only, never its window, selection or URL.
        guard trusted, !Self.sensitive(bid) else { return out }
        let root = AXUIElementCreateApplication(app.processIdentifier)
        AXUIElementSetMessagingTimeout(root, 0.25)   // never block activation on a hung app
        if focus, let w = attr(root, kAXFocusedWindowAttribute) {
            let win = w as! AXUIElement
            if let t = attr(win, kAXTitleAttribute) as? String, !t.isEmpty { out["window"] = t }
            if let doc = attr(win, "AXDocument") as? String, doc.hasPrefix("http") { out["url"] = doc }
        }
        if let f = attr(root, kAXFocusedUIElementAttribute) {
            let el = f as! AXUIElement
            if (attr(el, kAXSubroleAttribute) as? String) == "AXSecureTextField" { return out }
            if highlight, let sel = attr(el, kAXSelectedTextAttribute) as? String, !sel.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                out["selection"] = String(sel.prefix(4000))
            }
            if focus, out["url"] == nil, let u = attr(el, "AXURL") { out["url"] = (u as? URL)?.absoluteString ?? (u as? String) }
        }
        return out
    }

    /// "Point and ask": the window under the cursor (not just the frontmost app), read by the
    /// Sam Computer Use helper (it holds the Accessibility grant). `point` is in AX coordinates
    /// (top-left origin); default = the current mouse location. Bounded to 600 ms.
    static func pointed(at point: CGPoint? = nil) -> [String: String] {
        let p = point ?? {
            let m = NSEvent.mouseLocation
            let top = NSScreen.screens.first?.frame.maxY ?? 0   // primary screen: AX origin is its top-left
            return CGPoint(x: m.x, y: top - m.y)
        }()
        var req = URLRequest(url: URL(string: "http://127.0.0.1:47803/window_at")!, timeoutInterval: 0.6)
        LocalAuth.sign(&req)
        req.httpMethod = "POST"
        req.httpBody = try? JSONSerialization.data(withJSONObject: ["x": p.x, "y": p.y, "max_chars": 6000])
        let sem = DispatchSemaphore(value: 0)
        var out: [String: String] = [:]
        URLSession.shared.dataTask(with: req) { d, _, _ in
            if let d, let j = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any], j["error"] == nil {
                out["app"] = j["app"] as? String
                out["bundle"] = j["bundle"] as? String
                out["window"] = j["window"] as? String
                out["text"] = j["text"] as? String
                // The file shown (Preview…) or pointed at (Finder), and a web page's URL.
                out["file"] = (j["document"] as? String) ?? (j["file"] as? String)
                out["url"] = j["url"] as? String
            }
            sem.signal()
        }.resume()
        _ = sem.wait(timeout: .now() + 0.7)
        return out.filter { !$0.value.isEmpty }
    }

    /// Short label for the context chip ("Safari · selection").
    static func chip(_ c: [String: String]) -> String? {
        guard let app = c["app"] else { return nil }
        if c["selection"] != nil { return "\(app) · selection" }
        if let f = c["file"] { return "\(app) · \((f as NSString).lastPathComponent.prefix(28))" }
        if let w = c["window"], c["text"] != nil { return "\(app) · \(w.prefix(28))" }
        return app
    }
}
