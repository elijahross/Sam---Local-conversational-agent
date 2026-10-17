// Dictation: the cleaned transcript goes into the text field that had focus when the key went
// down. AX "selected text" replace (undoable with ⌘Z in native fields); else paste into that
// app (pasteboard restored afterwards); without Accessibility, copy + tell the user.
import AppKit
import ApplicationServices

struct DictationTarget {
    let pid: pid_t
    let app: String
    let element: AXUIElement?

    /// The focused element of the frontmost app (or of `bundle` in tests: never the user's apps).
    static func capture(bundle: String? = nil) -> DictationTarget? {
        let ra: NSRunningApplication? = bundle.flatMap { b in NSRunningApplication.runningApplications(withBundleIdentifier: b).first } ?? NSWorkspace.shared.frontmostApplication
        guard let ra, ra.bundleIdentifier != Bundle.main.bundleIdentifier else { return nil }
        var el: AnyObject?
        let root = AXUIElementCreateApplication(ra.processIdentifier)
        AXUIElementSetMessagingTimeout(root, 0.3)
        let ok = AXIsProcessTrusted() && AXUIElementCopyAttributeValue(root, kAXFocusedUIElementAttribute as CFString, &el) == .success
        return DictationTarget(pid: ra.processIdentifier, app: ra.localizedName ?? "app", element: ok ? (el as! AXUIElement) : nil)
    }

    enum Outcome { case inserted, pasted, copied }

    private func value(_ el: AXUIElement) -> String? {
        var v: AnyObject?
        return AXUIElementCopyAttributeValue(el, kAXValueAttribute as CFString, &v) == .success ? v as? String : nil
    }

    @MainActor
    func insert(_ text: String) -> Outcome {
        guard !text.isEmpty else { return .inserted }
        // Web views may "accept" the AX replace and do nothing: only count it if the value changed.
        if let el = element {
            let before = value(el)
            if AXUIElementSetAttributeValue(el, kAXSelectedTextAttribute as CFString, text as CFString) == .success {
                let after = value(el)
                if after != before, after?.contains(text.prefix(12)) ?? true { return .inserted }
            }
        }
        let pb = NSPasteboard.general
        let saved = pb.string(forType: .string)
        pb.clearContents(); pb.setString(text, forType: .string)
        guard AXIsProcessTrusted() else { return .copied }
        // ⌘V into the target app only (not whatever is frontmost now).
        let src = CGEventSource(stateID: .combinedSessionState)
        for down in [true, false] {
            let e = CGEvent(keyboardEventSource: src, virtualKey: 9, keyDown: down)   // kVK_ANSI_V
            e?.flags = .maskCommand
            e?.postToPid(pid)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
            if let saved { pb.clearContents(); pb.setString(saved, forType: .string) }
        }
        return .pasted
    }
}
