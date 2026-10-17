// Sam Computer Use: a tiny permissioned sidecar (127.0.0.1:47803, JSON over HTTP).
// macOS grants Accessibility / Screen Recording to THIS app, not to the terminal or IDE.
//
// GET  /health                         → {ax, screen}
// POST /open      {app}                → launch/activate an app by name or bundle id
// POST /snapshot  {app, max?}          → compact element list of the app's focused window (+menu bar opt.)
// POST /act       {id, action, value?} → press | set | focus | confirm | cancel | showmenu (no cursor movement)
// POST /keys      {app, text|combo}    → type text / key combo into THAT app's process only (postToPid)
// POST /screenshot{app, max_width?}    → JPEG (base64) of the app's front window + its frame
// POST /click     {app, x, y, double?} → real mouse click (cursor takeover), only if app is frontmost
// POST /open_file {path, app?} · /open_url {url} (http/https/settings/mailto) · /save_as {app, path} (AppleScript)
// POST /media     {key}                → the keyboard's media key (playpause | next | previous), system-wide
// POST /youtube   {url?, action}       → open a youtube.com page in the default browser and press its player's Play; pause | next | previous
// Only binds to localhost. No file or network access beyond that.
import AppKit
import ApplicationServices
import Foundation
import MapKit
import CoreLocation
import Network
import ScreenCaptureKit
import Security

setvbuf(stdout, nil, _IONBF, 0)

// MARK: - Accessibility snapshot

final class Snap {
    var elements: [String: AXUIElement] = [:]
    var counter = 0
}
let snap = Snap()
/// Input events sent / refused (refusals = beeps avoided). GET /stats.
var stats: [String: Int] = ["keys_sent": 0, "text_sent": 0, "clicks_sent": 0, "refused_no_text_focus": 0, "refused_not_front": 0, "dismissed": 0]
let lock = NSLock()

func attr(_ e: AXUIElement, _ a: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(e, a as CFString, &v) == .success ? v : nil
}
func str(_ e: AXUIElement, _ a: String) -> String {
    guard let v = attr(e, a) else { return "" }
    if let s = v as? String { return s }
    if let n = v as? NSNumber { return n.stringValue }
    return ""
}
func actions(_ e: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(e, &names) == .success, let a = names as? [String] else { return [] }
    return a
}
func frame(_ e: AXUIElement) -> CGRect? {
    guard let p = attr(e, kAXPositionAttribute), let s = attr(e, kAXSizeAttribute) else { return nil }
    var pt = CGPoint.zero, sz = CGSize.zero
    AXValueGetValue(p as! AXValue, .cgPoint, &pt)
    AXValueGetValue(s as! AXValue, .cgSize, &sz)
    return CGRect(origin: pt, size: sz)
}

let interesting: Set<String> = ["AXButton", "AXTextField", "AXTextArea", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXComboBox",
    "AXMenuItem", "AXMenuBarItem", "AXLink", "AXTab", "AXSlider", "AXStaticText", "AXCell", "AXRow", "AXSearchField", "AXSegmentedControl",
    "AXDisclosureTriangle", "AXMenuButton", "AXIncrementor", "AXImage", "AXHeading", "AXSwitch", "AXToggle"]

func app(named q: String) -> NSRunningApplication? {
    let ql = q.lowercased()
    return NSWorkspace.shared.runningApplications.first { ($0.localizedName ?? "").lowercased() == ql || ($0.bundleIdentifier ?? "").lowercased() == ql }
}

func snapshot(_ name: String, max: Int, menus: Bool) -> [String: Any] {
    guard let ra = app(named: name) else { return ["error": "app not running: \(name)"] }
    if isSensitive(ra.bundleIdentifier) { return ["error": "Sam doesn't operate password managers"] }
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    var lines: [String] = []
    var map: [String: AXUIElement] = [:]
    var n = 0
    var visited = 0
    func walk(_ e: AXUIElement, depth: Int) {
        if lines.count >= max || visited > 4000 || depth > 40 { return }
        visited += 1
        let role = str(e, kAXRoleAttribute)
        if role == "AXMenuBar" && !menus { return }
        let title = str(e, kAXTitleAttribute)
        let desc = str(e, kAXDescriptionAttribute)
        let help = str(e, kAXHelpAttribute)
        var value = isSecureField(e) ? (str(e, kAXValueAttribute).isEmpty ? "" : "••••") : str(e, kAXValueAttribute)
        let enabled = (attr(e, kAXEnabledAttribute) as? Bool) ?? true
        let acts = actions(e)
        let label = [title, desc].filter { !$0.isEmpty }.joined(separator: " / ")
        let useful = interesting.contains(role) && (!label.isEmpty || !value.isEmpty || role == "AXTextField" || role == "AXTextArea" || role == "AXSearchField")
        if useful && !(role == "AXStaticText" && value.isEmpty && label.isEmpty) {
            n += 1
            let id = "e\(n)"
            map[id] = e
            if value.count > 80 { value = String(value.prefix(80)) + "…" }
            var line = "\(id) \(role.replacingOccurrences(of: "AX", with: ""))"
            if !label.isEmpty { line += " \"\(label.prefix(60))\"" }
            if !value.isEmpty && value != label { line += " = \"\(value)\"" }
            if !help.isEmpty && label.isEmpty { line += " (\(help.prefix(40)))" }
            if !enabled { line += " [disabled]" }
            if acts.contains("AXPress") == false && role != "AXStaticText" && role != "AXTextField" && role != "AXTextArea" { line += " [no press]" }
            lines.append(line)
        }
        if let kids = attr(e, kAXChildrenAttribute) as? [AXUIElement] {
            for k in kids { walk(k, depth: depth + 1) }
        }
    }
    let win = attr(root, kAXFocusedWindowAttribute).map { $0 as! AXUIElement } ?? (attr(root, kAXWindowsAttribute) as? [AXUIElement])?.first
    if let w = win {
        walk(w, depth: 0)
        // Sheets / dialogs are children of the window and get walked too.
    }
    if menus, let mb = attr(root, kAXMenuBarAttribute) { walk(mb as! AXUIElement, depth: 0) }
    lock.lock(); snap.elements = map; lock.unlock()
    return ["app": ra.localizedName ?? name, "window": win.map { str($0, kAXTitleAttribute) } ?? "", "elements": lines.joined(separator: "\n"), "count": n, "visited": visited]
}

func act(_ id: String, _ action: String, _ value: String?) -> [String: Any] {
    lock.lock(); let e = snap.elements[id]; lock.unlock()
    guard let el = e else { return ["error": "unknown element \(id) (take a new snapshot)"] }
    var err: AXError = .success
    switch action {
    case "press": err = AXUIElementPerformAction(el, kAXPressAction as CFString)
    case "confirm": err = AXUIElementPerformAction(el, kAXConfirmAction as CFString)
    case "cancel": err = AXUIElementPerformAction(el, kAXCancelAction as CFString)
    case "showmenu": err = AXUIElementPerformAction(el, kAXShowMenuAction as CFString)
    case "focus": err = AXUIElementSetAttributeValue(el, kAXFocusedAttribute as CFString, kCFBooleanTrue)
    case "set":
        _ = AXUIElementSetAttributeValue(el, kAXFocusedAttribute as CFString, kCFBooleanTrue)
        err = AXUIElementSetAttributeValue(el, kAXValueAttribute as CFString, (value ?? "") as CFString)
    case "insert":
        // Through the app's text input (insertText via the selected-text range): apps whose own
        // model ignores a raw AXValue set (Catalyst / SwiftUI composers) register this.
        _ = AXUIElementSetAttributeValue(el, kAXFocusedAttribute as CFString, kCFBooleanTrue)
        err = AXUIElementSetAttributeValue(el, kAXSelectedTextAttribute as CFString, (value ?? "") as CFString)
    case "replace":
        // Targeted edit: select exactly the old text's range, replace the selection. Keeps the
        // document's formatting and touches nothing else (no cursor, no keyboard).
        guard let v = value, let data = v.data(using: .utf8), let o = (try? JSONSerialization.jsonObject(with: data)) as? [String: String],
              let old = o["old"], let new = o["new"], !old.isEmpty else { return ["error": "replace needs {old, new}"] }
        let full = (attr(el, kAXValueAttribute) as? String ?? "") as NSString
        let r = full.range(of: old)
        guard r.location != NSNotFound else { return ["error": "text not found: \(old.prefix(40))"] }
        var cf = CFRange(location: r.location, length: r.length)
        guard let rv = AXValueCreate(.cfRange, &cf) else { return ["error": "range"] }
        _ = AXUIElementSetAttributeValue(el, kAXFocusedAttribute as CFString, kCFBooleanTrue)
        err = AXUIElementSetAttributeValue(el, kAXSelectedTextRangeAttribute as CFString, rv)
        if err == .success { err = AXUIElementSetAttributeValue(el, kAXSelectedTextAttribute as CFString, new as CFString) }
        if err == .success, !((attr(el, kAXValueAttribute) as? String ?? "").contains(new)) { return ["error": "replace not applied"] }
    default: return ["error": "unknown action \(action)"]
    }
    return err == .success ? ["ok": true] : ["error": "AX error \(err.rawValue)"]
}

// MARK: - Keyboard / mouse (CGEvent)

let keyCodes: [String: CGKeyCode] = ["return": 36, "enter": 76, "tab": 48, "space": 49, "delete": 51, "escape": 53, "esc": 53,
    "left": 123, "right": 124, "down": 125, "up": 126, "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
    "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "o": 31, "u": 32, "i": 34, "p": 35, "l": 37, "j": 38, "k": 40,
    "n": 45, "m": 46, "1": 18, "2": 19, "3": 20, "4": 21, "5": 23, "6": 22, "7": 26, "8": 28, "9": 25, "0": 29, "=": 24, "-": 27,
    "/": 44, ".": 47, ",": 43]

// Keyboard events go to ONE process (postToPid), never to whatever happens to be frontmost.
func typeText(_ s: String, pid: pid_t) {
    let src = CGEventSource(stateID: .hidSystemState)
    for ch in s {
        let u = Array(String(ch).utf16)
        if let d = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: true), let up = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: false) {
            d.keyboardSetUnicodeString(stringLength: u.count, unicodeString: u)
            up.keyboardSetUnicodeString(stringLength: u.count, unicodeString: u)
            d.postToPid(pid); up.postToPid(pid)
        }
        usleep(8000)
    }
}

func combo(_ c: String, pid: pid_t) -> [String: Any] {
    let parts = c.lowercased().split(separator: "+").map(String.init)
    guard let key = parts.last, let code = keyCodes[key] else { return ["error": "unknown key in \(c)"] }
    var flags: CGEventFlags = []
    for m in parts.dropLast() {
        switch m {
        case "cmd", "command": flags.insert(.maskCommand)
        case "shift": flags.insert(.maskShift)
        case "alt", "option": flags.insert(.maskAlternate)
        case "ctrl", "control": flags.insert(.maskControl)
        default: return ["error": "unknown modifier \(m)"]
        }
    }
    let src = CGEventSource(stateID: .hidSystemState)
    let d = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: true)!
    let u = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: false)!
    d.flags = flags; u.flags = flags
    d.postToPid(pid); u.postToPid(pid)
    return ["ok": true]
}

// Mouse clicks need the real cursor: refused unless the target app is verifiably frontmost.
func click(_ x: Double, _ y: Double, double: Bool, app name: String) -> [String: Any] {
    guard let ra = app(named: name) else { return ["error": "app not running: \(name)"] }
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier else {
        stats["refused_not_front", default: 0] += 1
        return ["error": "\(name) is not frontmost (front: \(NSWorkspace.shared.frontmostApplication?.localizedName ?? "?")); click refused"]
    }
    let p = CGPoint(x: x, y: y)
    stats["clicks_sent", default: 0] += 1
    let src = CGEventSource(stateID: .hidSystemState)
    CGEvent(mouseEventSource: src, mouseType: .mouseMoved, mouseCursorPosition: p, mouseButton: .left)?.post(tap: .cghidEventTap)
    usleep(30000)
    for i in 0..<(double ? 2 : 1) {
        let d = CGEvent(mouseEventSource: src, mouseType: .leftMouseDown, mouseCursorPosition: p, mouseButton: .left)!
        let u = CGEvent(mouseEventSource: src, mouseType: .leftMouseUp, mouseCursorPosition: p, mouseButton: .left)!
        d.setIntegerValueField(.mouseEventClickState, value: Int64(i + 1)); u.setIntegerValueField(.mouseEventClickState, value: Int64(i + 1))
        d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap)
        usleep(60000)
    }
    return ["ok": true]
}

// MARK: - Screenshot (ScreenCaptureKit, front window of an app)

func screenshot(_ name: String, maxWidth: Int, anyLayer: Bool = false, largest: Bool = false, title: String? = nil) async -> [String: Any] {
    guard let ra = app(named: name) else { return ["error": "app not running: \(name)"] }
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
        let wins = content.windows.filter { $0.owningApplication?.processID == ra.processIdentifier && (anyLayer || $0.windowLayer == 0) && $0.frame.width > 80 }
        let wins2 = title.map { t in wins.filter { $0.title == t } } ?? wins
        guard let w = largest ? wins2.max(by: { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }) : wins2.first else { return ["error": "no visible window for \(name)"] }
        let cfg = SCStreamConfiguration()
        let scale = min(1.0, Double(maxWidth) / Double(w.frame.width))
        cfg.width = Int(w.frame.width * scale); cfg.height = Int(w.frame.height * scale)
        cfg.showsCursor = false
        let img = try await SCScreenshotManager.captureImage(contentFilter: SCContentFilter(desktopIndependentWindow: w), configuration: cfg)
        let rep = NSBitmapImageRep(cgImage: img)
        guard let jpg = rep.representation(using: .jpeg, properties: [.compressionFactor: 0.7]) else { return ["error": "encode"] }
        return ["jpeg_b64": jpg.base64EncodedString(), "frame": ["x": w.frame.origin.x, "y": w.frame.origin.y, "w": w.frame.width, "h": w.frame.height], "px": ["w": cfg.width, "h": cfg.height], "bytes": jpg.count]
    } catch {
        return ["error": "screen capture failed: \(error.localizedDescription)"]
    }
}

// Deterministic routes first (vault "API-first ladder"): no dialogs, no pixels.
func runOpen(_ args: [String]) -> [String: Any] {
    let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/open"); p.arguments = args
    do { try p.run(); p.waitUntilExit() } catch { return ["error": "\(error)"] }
    usleep(900000)
    return p.terminationStatus == 0 ? ["ok": true] : ["error": "open failed (\(p.terminationStatus))"]
}

/// Saves the app's front document to a POSIX path through the app's scripting dictionary.
func saveAs(_ name: String, _ path: String) -> [String: Any] {
    guard let ra = app(named: name), let bid = ra.bundleIdentifier else { return ["error": "app not running: \(name)"] }
    let esc = path.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    // An app with an open sheet ignores Apple events until it closes: bounded, never a hang.
    let src = "with timeout of 15 seconds\ntell application id \"\(bid)\" to save front document in (POSIX file \"\(esc)\")\nend timeout"
    var err: NSDictionary?
    guard let script = NSAppleScript(source: src) else { return ["error": "script"] }
    script.executeAndReturnError(&err)
    if let e = err { return ["error": "save failed: \(e[NSAppleScript.errorMessage] ?? e)"] }
    return ["ok": true]
}

/// Brings an app to the front through LaunchServices and verifies it (keystrokes that must go
/// through the HID tap — e.g. into out-of-process Save panels — are only sent after this).
func frontVerified(_ ra: NSRunningApplication) -> Bool {
    for attempt in 0..<2 {
        if NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier { return true }
        _ = runOpen(ra.bundleIdentifier.map { ["-b", $0] } ?? ["-a", ra.localizedName ?? ""])
        if attempt == 1 { ra.activate() }
        for _ in 0..<25 {
            if NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier { return true }
            usleep(100000)
        }
    }
    return false
}

func hidCombo(_ c: String) {
    let parts = c.lowercased().split(separator: "+").map(String.init)
    guard let key = parts.last, let code = keyCodes[key] else { return }
    var flags: CGEventFlags = []
    for m in parts.dropLast() {
        if m == "cmd" { flags.insert(.maskCommand) } else if m == "shift" { flags.insert(.maskShift) } else if m == "alt" { flags.insert(.maskAlternate) }
    }
    let src = CGEventSource(stateID: .hidSystemState)
    let d = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: true)!, u = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: false)!
    d.flags = flags; u.flags = flags
    d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap)
}

func hidType(_ s: String) {
    let src = CGEventSource(stateID: .hidSystemState)
    for ch in s {
        let u = Array(String(ch).utf16)
        if let d = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: true), let up = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: false) {
            d.keyboardSetUnicodeString(stringLength: u.count, unicodeString: u); up.keyboardSetUnicodeString(stringLength: u.count, unicodeString: u)
            d.post(tap: .cghidEventTap); up.post(tap: .cghidEventTap)
        }
        usleep(6000)
    }
}

func sheets(_ ra: NSRunningApplication) -> [AXUIElement] {
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    guard let w = attr(root, kAXFocusedWindowAttribute) else { return [] }
    var out: [AXUIElement] = []
    // Save/Open panels are often reported as the focused window itself (a sheet/dialog with
    // a default button), not as a child sheet of the document window.
    let fw = w as! AXUIElement
    if str(fw, kAXRoleAttribute) == "AXSheet" || str(fw, kAXSubroleAttribute) == "AXDialog" || attr(fw, kAXDefaultButtonAttribute) != nil { out.append(fw) }
    func walk(_ e: AXUIElement, _ d: Int) {
        if d > 3 { return }
        for k in (attr(e, kAXChildrenAttribute) as? [AXUIElement]) ?? [] {
            if str(k, kAXRoleAttribute) == "AXSheet" { out.append(k) }
            walk(k, d + 1)
        }
    }
    walk(w as! AXUIElement, 0)
    return out
}

func findAll(_ e: AXUIElement, role: String, depth: Int = 0) -> [AXUIElement] {
    if depth > 40 { return [] }
    var out: [AXUIElement] = []
    if str(e, kAXRoleAttribute) == role { out.append(e) }
    for k in (attr(e, kAXChildrenAttribute) as? [AXUIElement]) ?? [] { out += findAll(k, role: role, depth: depth + 1) }
    return out
}

// MARK: - Window under the cursor ("point and ask")

/// The window at a screen point (AX, top-left origin), never Sam's own windows, and its text in
/// reading order (static texts, text areas, web content), deduplicated and capped.
func windowAt(x: Double, y: Double, maxChars: Int) -> [String: Any] {
    let sys = AXUIElementCreateSystemWide()
    var hit: AXUIElement?
    guard AXUIElementCopyElementAtPosition(sys, Float(x), Float(y), &hit) == .success, var e = hit else { return ["error": "nothing at \(x),\(y)"] }
    var pid: pid_t = 0
    AXUIElementGetPid(e, &pid)
    if let me = NSRunningApplication.current.processIdentifier as pid_t?, pid == me { return ["error": "point is on Sam itself"] }
    // Sam's own windows (island, overview, connectors) are never "what the user points at":
    // take the topmost other app's window under the point instead.
    if isSamWindow(pid) {
        guard let under = windowBelowSam(x: x, y: y) else { return ["error": "only Sam's own window is at \(x),\(y)"] }
        pid = under.pid
        e = under.window
    }
    // What the pointer is on, beyond text: a file item (Finder icon / list row) and a web page URL.
    var extra: [String: Any] = [:]
    var cur: AXUIElement? = e
    for _ in 0..<14 {
        guard let c = cur else { break }
        if let raw = attr(c, kAXURLAttribute), let u = (raw as? URL) ?? (raw as? String).flatMap(URL.init(string:)) {
            if u.isFileURL, extra["file"] == nil { extra["file"] = u.path }
            else if u.scheme?.hasPrefix("http") == true, extra["url"] == nil { extra["url"] = u.absoluteString }
        }
        cur = attr(c, kAXParentAttribute).map { $0 as! AXUIElement }
    }
    // The window: the hit element's AXWindow attribute (web views rendered in another process
    // have a parent chain that ends early — Mail's message body), else walk up, else the app
    // window whose frame contains the point.
    if let w = attr(e, kAXWindowAttribute) { e = w as! AXUIElement }
    for _ in 0..<60 where str(e, kAXRoleAttribute) != "AXWindow" {
        guard let p = attr(e, kAXParentAttribute) else { break }
        e = p as! AXUIElement
    }
    if str(e, kAXRoleAttribute) != "AXWindow" {
        let wins = (attr(AXUIElementCreateApplication(pid), kAXWindowsAttribute) as? [AXUIElement]) ?? []
        if let w = wins.first(where: { frame($0)?.contains(CGPoint(x: x, y: y)) == true }) { e = w }
    }
    // The document a window shows (Preview, TextEdit, Pages…): AXDocument is its file URL.
    if let d = attr(e, "AXDocument") as? String, let u = URL(string: d), u.isFileURL { extra["document"] = u.path }
    return readWindowText(e, pid: pid, maxChars: maxChars).merging(extra) { a, _ in a }
}

/// Sam's own processes (app "one.sam.app" and this helper).
func isSamWindow(_ pid: pid_t) -> Bool {
    guard let b = NSRunningApplication(processIdentifier: pid)?.bundleIdentifier else { return false }
    return b.hasPrefix("one.sam.")
}

/// The frontmost non-Sam window containing a point (CG window list, front to back), as an AX
/// window of its app (matched by frame).
func windowBelowSam(x: Double, y: Double) -> (pid: pid_t, window: AXUIElement)? {
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] else { return nil }
    for w in list {
        guard let pid = w[kCGWindowOwnerPID as String] as? pid_t, (w[kCGWindowLayer as String] as? Int) == 0,
              let b = w[kCGWindowBounds as String] as? [String: CGFloat], let r = CGRect(dictionaryRepresentation: b as CFDictionary),
              r.contains(CGPoint(x: x, y: y)), !isSamWindow(pid) else { continue }
        let wins = (attr(AXUIElementCreateApplication(pid), kAXWindowsAttribute) as? [AXUIElement]) ?? []
        if let ax = wins.first(where: { frame($0).map { abs($0.minX - r.minX) < 4 && abs($0.minY - r.minY) < 4 } ?? false }) ?? wins.first(where: { frame($0)?.contains(CGPoint(x: x, y: y)) == true }) {
            return (pid, ax)
        }
    }
    return nil
}

/// Password managers and credential stores: Sam never reads their windows (point-and-ask or
/// computer use). Their window title can name an account, so it isn't returned either.
let SENSITIVE_APPS: [String] = ["com.apple.keychainaccess", "com.apple.Passwords", "com.agilebits.onepassword", "com.1password.", "com.bitwarden.",
                                "com.lastpass.", "com.dashlane.", "org.keepassxc.", "com.enpass.", "in.sinew.Enpass", "com.nordpass.", "com.protonpass.", "me.proton.pass"]
func isSensitive(_ bundle: String?) -> Bool {
    guard let b = bundle?.lowercased(), !b.isEmpty else { return false }
    return SENSITIVE_APPS.contains { b == $0.lowercased() || b.hasPrefix($0.lowercased()) }
}
/// A password field (its value is a secret even when the app exposes it).
func isSecureField(_ e: AXUIElement) -> Bool { str(e, kAXSubroleAttribute) == "AXSecureTextField" }

/// Text of one window in reading order — works for covered/background windows too (no raise).
func readWindowText(_ e: AXUIElement, pid: pid_t, maxChars: Int) -> [String: Any] {
    let app = NSRunningApplication(processIdentifier: pid)
    if isSensitive(app?.bundleIdentifier) {
        return ["app": app?.localizedName ?? "?", "bundle": app?.bundleIdentifier ?? "", "pid": pid, "window": "", "text": "", "chars": 0, "withheld": "password manager: Sam doesn't read it"]
    }
    var parts: [String] = []
    var seen = Set<String>()
    var total = 0
    let t0 = Date()
    func walk(_ el: AXUIElement, _ depth: Int) {
        if total >= maxChars || depth > 60 || Date().timeIntervalSince(t0) > 2.5 { return }
        let role = str(el, kAXRoleAttribute)
        if ["AXButton", "AXMenuBar", "AXToolbar", "AXScrollBar", "AXImage", "AXMenu"].contains(role) { return }
        if isSecureField(el) { return }
        var texts: [String] = []
        if ["AXStaticText", "AXTextArea", "AXTextField", "AXHeading", "AXLink", "AXCell"].contains(role) {
            texts.append(str(el, kAXValueAttribute))
            if role == "AXLink" { texts.append(str(el, kAXTitleAttribute)) }
        }
        if role == "AXWindow" { texts.append(str(el, kAXTitleAttribute)) }
        for t in texts {
            let c = t.trimmingCharacters(in: .whitespacesAndNewlines)
            if c.count > 1, !seen.contains(c) { seen.insert(c); parts.append(c); total += c.count }
        }
        for k in (attr(el, kAXChildrenAttribute) as? [AXUIElement]) ?? [] { walk(k, depth + 1) }
    }
    AXUIElementSetMessagingTimeout(AXUIElementCreateApplication(pid), 0.5)
    walk(e, 0)
    let text = parts.joined(separator: "\n")
    return ["app": app?.localizedName ?? "?", "bundle": app?.bundleIdentifier ?? "", "pid": pid, "window": str(e, kAXTitleAttribute),
            "text": String(text.prefix(maxChars)), "chars": text.count, "ms": Int(Date().timeIntervalSince(t0) * 1000)]
}

/// Frame of an app's front window (AX coords) — lets tests "point" at it without moving the mouse.
func windowFrame(_ name: String, title: String?) -> [String: Any] {
    guard let ra = app(named: name) else { return ["error": "app not running"] }
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    let wins = (attr(root, kAXWindowsAttribute) as? [AXUIElement]) ?? []
    let pick = title.flatMap { t in wins.first { str($0, kAXTitleAttribute).localizedCaseInsensitiveContains(t) } }
        ?? (attr(root, kAXFocusedWindowAttribute).map { $0 as! AXUIElement }) ?? wins.first
    guard let w = pick, let f = frame(w) else { return ["error": "no window", "titles": wins.map { str($0, kAXTitleAttribute) }] }
    return ["x": f.minX, "y": f.minY, "w": f.width, "h": f.height, "title": str(w, kAXTitleAttribute), "titles": wins.map { str($0, kAXTitleAttribute) }]
}

// MARK: - Rich documents (file-first: no UI formatting)

/// HTML (with local <img> files and <a href> links) → RTFD / DOCX through AppKit's own rich-text
/// engine: images become real attachments, links stay clickable. textutil dropped both (lab).
@MainActor
func makeDocMain(_ htmlPath: String, _ out: String, _ format: String) -> [String: Any] {
    let src = URL(fileURLWithPath: htmlPath)
    guard let data = try? Data(contentsOf: src) else { return ["error": "cannot read \(htmlPath)"] }
    var attrs: NSDictionary? = nil
    guard let doc = NSMutableAttributedString(html: data, baseURL: src.deletingLastPathComponent(), documentAttributes: &attrs) else { return ["error": "HTML import failed"] }
    // Images: make sure every attachment carries its image data (file-URL images can arrive
    // as lazy references), scaled to the page width.
    var images = 0, links = 0
    doc.enumerateAttribute(.attachment, in: NSRange(location: 0, length: doc.length)) { v, range, _ in
        guard let att = v as? NSTextAttachment else { return }
        var img: NSImage? = att.image
        if img == nil, let fw = att.fileWrapper, let d = fw.regularFileContents { img = NSImage(data: d) }
        if img == nil, let u = att.fileWrapper?.filename.map({ src.deletingLastPathComponent().appendingPathComponent($0) }) { img = NSImage(contentsOf: u) }
        guard let im = img, let tiff = im.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff), let jpg = rep.representation(using: .jpeg, properties: [.compressionFactor: 0.8]) else { return }
        let fw = FileWrapper(regularFileWithContents: jpg); fw.preferredFilename = "image\(images + 1).jpg"
        let na = NSTextAttachment(fileWrapper: fw)
        let w = min(420, im.size.width); na.bounds = CGRect(x: 0, y: 0, width: w, height: im.size.height * w / max(im.size.width, 1))
        doc.addAttribute(.attachment, value: na, range: range)
        images += 1
    }
    doc.enumerateAttribute(.link, in: NSRange(location: 0, length: doc.length)) { v, _, _ in if v != nil { links += 1 } }
    let range = NSRange(location: 0, length: doc.length)
    let outURL = URL(fileURLWithPath: out)
    do {
        switch format {
        case "rtfd":
            guard let fw = doc.rtfdFileWrapper(from: range, documentAttributes: [.documentType: NSAttributedString.DocumentType.rtfd]) else { return ["error": "RTFD export failed"] }
            try fw.write(to: outURL, options: .atomic, originalContentsURL: nil)
        case "docx":
            let d = try doc.data(from: range, documentAttributes: [.documentType: NSAttributedString.DocumentType.officeOpenXML])
            try d.write(to: outURL, options: .atomic)
        default: return ["error": "format must be rtfd or docx"]
        }
    } catch { return ["error": "write failed: \(error.localizedDescription)"] }
    return ["ok": true, "out": out, "images": images, "links": links, "chars": doc.length]
}

func makeDoc(_ html: String, _ out: String, _ format: String) async -> [String: Any] {
    await MainActor.run { makeDocMain(html, out, format) }
}

// MARK: - Typed save errors (vault: "Typed Error Taxonomy with Possible Fixes")

func saveError(_ code: String, _ message: String, fix: String, recoverable: Bool, extra: [String: Any] = [:]) -> [String: Any] {
    var r: [String: Any] = ["error": message, "code": code, "fix": fix, "recoverable": recoverable]
    for (k, v) in extra { r[k] = v }
    return r
}

/// Classifies an alert's text (German + English macOS wording) into an error code.
func classifyAlert(_ text: String) -> String {
    let t = text.lowercased()
    let rules: [(String, [String])] = [
        ("exists", ["existiert bereits", "already exists", "ersetzen", "replace it"]),
        ("no_space", ["nicht genügend", "speicherplatz", "not enough", "disk is full", "volume is full", "ist voll", "out of space"]),
        ("read_only", ["schreibgeschützt", "read-only", "read only"]),
        ("locked", ["geschützt", "gesperrt", "is locked", "locked"]),
        ("permission", ["zugriffsrecht", "berechtigung", "nicht berechtigt", "permission", "not permitted", "keine rechte"]),
        ("invalid_name", ["ungültig", "invalid", "zu lang", "too long", "kann nicht verwendet", "can't be used", "cannot be used", "reserviert", "reserved", "mit einem punkt", "with a period"]),
    ]
    for (code, words) in rules where words.contains(where: { t.contains($0) }) { return code }
    return "unknown"
}

let fixes: [String: (String, Bool)] = [
    "exists": ("ask the user: overwrite (if_exists=overwrite, needs approval) or another name (if_exists=rename)", true),
    "permission": ("choose a folder the user can write to, or ask the user to grant access", true),
    "no_space": ("free space on the volume or choose another volume; tell the user how much is free", false),
    "invalid_name": ("use a shorter name without : or / (≤ 255 bytes) and retry once", true),
    "locked": ("the file is locked (Get Info → Locked); ask the user to unlock it or save under another name", false),
    "read_only": ("the volume is read-only; choose another folder", true),
    "folder_missing": ("create the folder first (needs approval) or pick an existing one", true),
    "focus_lost": ("the user switched apps; retry once when the app is in front", true),
    "timeout": ("the app didn't respond; retry once", true),
    "unknown": ("report macOS's message to the user", false),
]
func fail(_ code: String, _ message: String, _ extra: [String: Any] = [:]) -> [String: Any] {
    let f = fixes[code] ?? fixes["unknown"]!
    return saveError(code, message, fix: f.0, recoverable: f.1, extra: extra)
}

/// Pre-flight checks before any UI is touched. Returns an error, or nil plus the final name.
func preflight(dir: String, base: String, ifExists: String, minBytes: Int64) -> ([String: Any]?, String) {
    let fm = FileManager.default
    var isDir: ObjCBool = false
    guard fm.fileExists(atPath: dir, isDirectory: &isDir), isDir.boolValue else { return (fail("folder_missing", "folder does not exist: \(dir)"), base) }
    let url = URL(fileURLWithPath: dir)
    let rv = try? url.resourceValues(forKeys: [.volumeIsReadOnlyKey, .volumeAvailableCapacityForImportantUsageKey, .volumeAvailableCapacityKey])
    if rv?.volumeIsReadOnly == true { return (fail("read_only", "the volume of \(dir) is read-only"), base) }
    let free = rv?.volumeAvailableCapacityForImportantUsage ?? Int64(rv?.volumeAvailableCapacity ?? Int.max)
    if free < minBytes { return (fail("no_space", "not enough free space in \(dir): \(free / 1024) KB free, need ≥ \(minBytes / 1024) KB", ["free_bytes": free]), base) }
    guard access(dir, W_OK) == 0 else { return (fail("permission", "no write permission for \(dir)"), base) }
    if base.isEmpty || base.contains(":") || base.contains("/") || base.utf8.count > 255 || base.hasPrefix(".") {
        return (fail("invalid_name", "invalid file name: \(base.prefix(60))\(base.count > 60 ? "…" : "") (\(base.utf8.count) bytes)"), base)
    }
    let stem = (base as NSString).deletingPathExtension
    let existing = ((try? fm.contentsOfDirectory(atPath: dir)) ?? []).filter { ($0 as NSString).deletingPathExtension == stem }
    if let ex = existing.first {
        let exURL = URL(fileURLWithPath: dir).appendingPathComponent(ex)
        switch ifExists {
        case "overwrite":
            let locked = (try? exURL.resourceValues(forKeys: [.isUserImmutableKey]))?.isUserImmutable == true
            if locked { return (fail("locked", "\(ex) is locked"), base) }
            if access(exURL.path, W_OK) != 0 { return (fail("permission", "no write permission for \(ex)"), base) }
        case "rename":
            var n = 2, cand = "\(stem) \(n)"
            let names = Set(((try? fm.contentsOfDirectory(atPath: dir)) ?? []).map { ($0 as NSString).deletingPathExtension })
            while names.contains(cand) { n += 1; cand = "\(stem) \(n)" }
            return (nil, cand)
        default:
            return (fail("exists", "\(ex) already exists in \(dir)", ["existing": exURL.path]), base)
        }
    }
    return (nil, base)
}

/// Reads an alert's texts (titles + static texts) for classification / reporting.
func alertText(_ e: AXUIElement) -> String {
    (findAll(e, role: "AXStaticText").map { str($0, kAXValueAttribute) } + [str(e, kAXTitleAttribute), str(e, kAXDescriptionAttribute)])
        .filter { !$0.isEmpty }.joined(separator: " ")
}

/// Closes every open sheet of the app via Cancel/OK/Close buttons (AX only, no keys).
func closeAllSheets(_ ra: NSRunningApplication) {
    for _ in 0..<4 {
        guard let sh = sheets(ra).last else { return }
        let words = ["abbrechen", "cancel", "ok", "schließen", "close"]
        let b: AXUIElement? = attr(sh, kAXCancelButtonAttribute).map { $0 as! AXUIElement }
            ?? findAll(sh, role: "AXButton").first { words.contains(str($0, kAXTitleAttribute).lowercased()) }
            ?? attr(sh, kAXDefaultButtonAttribute).map { $0 as! AXUIElement }
        guard let btn = b else { return }
        AXUIElementPerformAction(btn, kAXPressAction as CFString); usleep(350000)
    }
}

/// Saves through the app's real Save panel (sandboxed apps only get write access to a folder
/// the Save panel picked). ⌘S → ⌘⇧G folder → name via AX → default button → verify on disk.
func saveDialog(_ name: String, _ path: String, ifExists: String, minBytes: Int64, skipPreflight: Bool = false) -> [String: Any] {
    guard let ra = app(named: name) else { return ["error": "app not running: \(name)", "code": "unknown"] }
    let url = URL(fileURLWithPath: (path as NSString).expandingTildeInPath)
    let dir = url.deletingLastPathComponent().path
    let (pre, base) = skipPreflight ? (nil, url.lastPathComponent) : preflight(dir: dir, base: url.lastPathComponent, ifExists: ifExists, minBytes: minBytes)
    if let e = pre { return e }  // nothing touched in the UI
    if skipPreflight, !FileManager.default.fileExists(atPath: dir) { return fail("folder_missing", "folder does not exist: \(dir)") }
    let overwrite = ifExists == "overwrite"
    let started = Date()
    guard frontVerified(ra) else { return fail("focus_lost", "could not bring \(name) to the front; nothing typed") }
    var sheet = sheets(ra).first
    // A leftover "Go to" box from an earlier attempt: close it first.
    if let s0 = sheet, findAll(s0, role: "AXButton").contains(where: { ["schließen", "close"].contains(str($0, kAXTitleAttribute).lowercased()) }) && findAll(s0, role: "AXTextField").count == 1 {
        if let c = findAll(s0, role: "AXButton").first(where: { ["schließen", "close"].contains(str($0, kAXTitleAttribute).lowercased()) }) { AXUIElementPerformAction(c, kAXPressAction as CFString); usleep(500000) }
        sheet = sheets(ra).first
    }
    if sheet == nil {
        hidCombo("cmd+s")
        for _ in 0..<30 { usleep(100000); if let s = sheets(ra).first { sheet = s; break } }
    }
    guard sheet != nil else { return fail("timeout", "no Save panel appeared (document may already be saved; use a new document)") }
    // Folder: Go-to sheet inside the panel.
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier else { return ["error": "focus moved away; aborted"] }
    // The Go-to box = a sheet with exactly one text field and no Save button (identified by
    // structure; its value gets normalized while typing, so matching the path is unreliable).
    let saveWords = ["sichern", "save", "speichern", "enregistrer", "guardar"]
    let goBox = { () -> AXUIElement? in
        sheets(ra).first { s in
            findAll(s, role: "AXTextField").count == 1 && !findAll(s, role: "AXButton").contains { saveWords.contains(str($0, kAXTitleAttribute).lowercased()) }
        }
    }
    let goOpen = { () -> Bool in goBox() != nil }
    // Path entry without typing (typed characters that land before the box is ready beep, one
    // per character — user report): wait for the panel, one ⌘⇧G, wait for the box's field,
    // then put the path in through Accessibility.
    var panelReady = false
    for _ in 0..<20 { if let p = sheets(ra).first, findAll(p, role: "AXTextField").contains(where: { !str($0, kAXValueAttribute).isEmpty }) { panelReady = true; break }; usleep(100000) }
    guard panelReady else { return ["error": "Save panel not ready; nothing typed"] }
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier else { return ["error": "focus moved away; aborted"] }
    hidCombo("cmd+shift+g")
    var goField: AXUIElement? = nil
    for _ in 0..<20 {
        usleep(100000)
        // The Go-to box is a new sheet whose single text field wasn't there before.
        if let box = goBox() { goField = findAll(box, role: "AXTextField").first; break }
    }
    guard let gf = goField else { return ["error": "Go-to box did not open; nothing typed"] }
    // Path via Accessibility (typing it char by char beeped — user report), then ONE Enter.
    AXUIElementSetAttributeValue(gf, kAXFocusedAttribute as CFString, kCFBooleanTrue)
    AXUIElementSetAttributeValue(gf, kAXValueAttribute as CFString, dir as CFString)
    usleep(350000)
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier else { return ["error": "focus moved away; nothing typed"] }
    hidCombo("return")
    for _ in 0..<20 { usleep(100000); if !goOpen() { break } }
    // Keyboard Return never reached the out-of-process box in the lab (R7–R9), so go straight to
    // the reliable route: double-click the suggestion once it appears; Return only as fallback.
    let lastComp = (dir as NSString).lastPathComponent
    let suggestion = { () -> CGRect? in
        guard let box = goBox() else { return nil }
        let texts = findAll(box, role: "AXStaticText")
        // The highlighted suggestion row is a breadcrumb ending in the folder name (lab R7–R10);
        // the full-path text above it is only a heading.
        let row = texts.last(where: { str($0, kAXValueAttribute) == lastComp })
        return row.flatMap { frame($0) }
    }
    // Enter, but only once the box has processed the typed path (its suggestion row appeared);
    // Return sent earlier did nothing (lab R7). Double-click on the suggestion is the fallback.
    var ready = false
    if goOpen() { for _ in 0..<30 { if suggestion() != nil { ready = true; break }; usleep(100000) } }
    if ready, goOpen(), NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier {
        usleep(150000)
        hidCombo("return")
        for _ in 0..<15 { usleep(100000); if !goOpen() { break } }
    }
    for _ in 0..<(goOpen() ? 2 : 0) {
        var rect: CGRect? = nil
        for _ in 0..<30 { if let r = suggestion() { rect = r; break }; usleep(100000) }
        guard let r = rect, NSWorkspace.shared.frontmostApplication?.processIdentifier == ra.processIdentifier else { break }
        _ = click(r.midX, r.midY, double: true, app: name)
        for _ in 0..<15 { usleep(100000); if !goOpen() { break } }
        if !goOpen() { break }
        usleep(400000)
    }
    if goOpen(), let gf2 = goBox().flatMap({ findAll($0, role: "AXTextField").first }) {
        // Last resort without keys: AX confirm on the field.
        AXUIElementPerformAction(gf2, kAXConfirmAction as CFString)
        for _ in 0..<10 { usleep(100000); if !goOpen() { break } }
    }
    if goOpen() {
        // Leave nothing half-open: close the Go-to box and cancel the Save panel (AX, no keys).
        for sh in sheets(ra).reversed() {
            let b: AXUIElement? = attr(sh, kAXCancelButtonAttribute).map { $0 as! AXUIElement }
                ?? findAll(sh, role: "AXButton").first { ["abbrechen", "cancel", "schließen", "close"].contains(str($0, kAXTitleAttribute).lowercased()) }
            if let c = b { AXUIElementPerformAction(c, kAXPressAction as CFString); usleep(300000) }
        }
        return fail(access(dir, W_OK) == 0 ? "timeout" : "permission", "could not navigate the Save panel to \(dir); panel cancelled, nothing saved")
    }
    usleep(150000)
    // Name: the panel's editable text field that isn't search/tags (re-read after navigation).
    guard let panel = sheets(ra).first else { return fail("unknown", "Save panel disappeared") }
    let fields = findAll(panel, role: "AXTextField").filter { (attr($0, kAXEnabledAttribute) as? Bool) ?? true }
    // The name field is the one pre-filled with the default name ("Untitled" / "Ohne Titel");
    // the search field is empty and unlabeled (lab: it got picked before).
    let candidates = fields.filter { f in
        let d = (str(f, kAXDescriptionAttribute) + str(f, kAXTitleAttribute)).lowercased()
        return !d.contains("tag") && !d.contains("such") && !d.contains("search") && str(f, kAXValueAttribute) != dir
    }
    guard let nameField = candidates.first(where: { !str($0, kAXValueAttribute).isEmpty }) else { closeAllSheets(ra); return fail("unknown", "name field not found; panel cancelled") }
    AXUIElementSetAttributeValue(nameField, kAXValueAttribute as CFString, base as CFString)
    usleep(200000)
    let def: AXUIElement? = attr(panel, kAXDefaultButtonAttribute).map { $0 as! AXUIElement }
        ?? findAll(panel, role: "AXButton").first { saveWords.contains(str($0, kAXTitleAttribute).lowercased()) }
    guard let saveButton = def else { closeAllSheets(ra); return fail("unknown", "save button not found; panel cancelled") }
    AXUIElementPerformAction(saveButton, kAXPressAction as CFString)
    usleep(800000)
    // After Save: either the file appears, or macOS shows an alert (in the panel or, after it
    // closed, on the document window). Alerts are read, classified, closed — never left open.
    let stem = (base as NSString).deletingPathExtension
    let fileNow = { () -> String? in
        ((try? FileManager.default.contentsOfDirectory(atPath: dir)) ?? []).first { f in
            guard (f as NSString).deletingPathExtension == stem else { return false }
            let a = try? FileManager.default.attributesOfItem(atPath: "\(dir)/\(f)")
            return ((a?[.modificationDate] as? Date) ?? .distantPast) >= started.addingTimeInterval(-2)
        }
    }
    for _ in 0..<30 {
        if let f = fileNow() {
            let size = ((try? FileManager.default.attributesOfItem(atPath: "\(dir)/\(f)"))?[.size] as? Int64) ?? 0
            if size == 0 { usleep(100000); continue }
            if sheets(ra).isEmpty { return ["ok": true, "saved": "\(dir)/\(f)", "bytes": size, "name": f] }
        }
        // An alert: a sheet that is not the Save panel itself (no name field) or a new dialog.
        if let alert = sheets(ra).last(where: { s in !alertText(s).isEmpty && (findAll(s, role: "AXTextField").isEmpty || classifyAlert(alertText(s)) != "unknown") }) {
            let text = alertText(alert)
            let code = classifyAlert(text)
            if code == "exists" && overwrite, let rep = findAll(alert, role: "AXButton").first(where: { ["ersetzen", "replace"].contains(str($0, kAXTitleAttribute).lowercased()) }) {
                AXUIElementPerformAction(rep, kAXPressAction as CFString); usleep(500000); continue
            }
            closeAllSheets(ra)
            return fail(code, "macOS: \(text.prefix(300))", ["alert": String(text.prefix(500))])
        }
        usleep(100000)
    }
    let leftover = sheets(ra).map(alertText).joined(separator: " ")
    closeAllSheets(ra)
    return fail(leftover.isEmpty ? "timeout" : classifyAlert(leftover), leftover.isEmpty ? "Save finished without a new file at \(dir)/\(base).*" : "macOS: \(leftover.prefix(300))")
}

/// Opens an app. `background`: launched without activation (`open -g`), so the user's focus
/// stays where it is; the agent works through AX and pid-targeted keys.
func openApp(_ name: String, background: Bool = false) -> [String: Any] {
    let byId = name.contains(".") && !name.contains(" ")
    let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    p.arguments = (background ? ["-g"] : []) + (byId ? ["-b", name] : ["-a", name])
    do { try p.run(); p.waitUntilExit() } catch { return ["error": "\(error)"] }
    usleep(900000)
    if !background { app(named: name)?.activate() } else { parkOnSecondScreen(name) }
    return p.terminationStatus == 0 ? ["ok": true, "pid": app(named: name)?.processIdentifier ?? 0] : ["error": "open failed"]
}

/// A real second screen: not the primary one (menu bar) and not Sam's own virtual displays
/// ("Sam Workspace", the tests' "Sam Workspace"). Nil when the Mac has one screen.
func secondScreen() -> NSScreen? {
    let primary = NSScreen.screens.first
    return NSScreen.screens.first { s in s != primary && !s.localizedName.contains("Workspace") && !s.localizedName.lowercased().contains("virtual") && s.frame.width >= 800 }
}

/// Background work opens apps for the agent; with a second screen, their windows go there so
/// nothing covers what the user is working on. AX positions are top-left relative to the
/// primary screen; NSScreen frames are bottom-left.
func parkOnSecondScreen(_ name: String) {
    guard let s = secondScreen(), let primary = NSScreen.screens.first, let ra = app(named: name) else { return }
    let wins = (attr(AXUIElementCreateApplication(ra.processIdentifier), kAXWindowsAttribute) as? [AXUIElement]) ?? []
    guard let w = wins.first else { return }
    if let f = frame(w), s.frame.contains(CGPoint(x: f.midX, y: primary.frame.maxY - f.midY)) { return }
    var pt = CGPoint(x: s.frame.minX + 60, y: (primary.frame.maxY - s.frame.maxY) + 80)
    if let v = AXValueCreate(.cgPoint, &pt) { AXUIElementSetAttributeValue(w, kAXPositionAttribute as CFString, v) }
}

// MARK: - HTTP (localhost only)

func handle(_ method: String, _ path: String, _ body: [String: Any]) async -> [String: Any] {
    switch (method, path) {
    case ("GET", "/health"): return ["ax": AXIsProcessTrusted(), "screen": CGPreflightScreenCaptureAccess()]
    case ("POST", "/request_access"):
        let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        let ax = AXIsProcessTrustedWithOptions(opts)
        let sc = CGRequestScreenCaptureAccess()
        return ["ax": ax, "screen": sc]
    case ("POST", "/open"): return openApp(body["app"] as? String ?? "", background: body["background"] as? Bool ?? false)
    case ("POST", "/chat_read"): return chatRead(body["app"] as? String ?? "")
    case ("POST", "/chat_clear"): return chatClear(body["app"] as? String ?? "", expect: body["expect"] as? String ?? "")
    case ("POST", "/chat_send"): return chatSend(body["app"] as? String ?? "", expect: body["expect"] as? String ?? "")
    case ("POST", "/snapshot"): return snapshot(body["app"] as? String ?? "", max: body["max"] as? Int ?? 150, menus: body["menus"] as? Bool ?? false)
    case ("POST", "/act"): return act(body["id"] as? String ?? "", body["action"] as? String ?? "press", body["value"] as? String)
    case ("POST", "/keys"):
        guard let ra = app(named: body["app"] as? String ?? "") else { return ["error": "keys need a running target app"] }
        if let t = body["text"] as? String {
            // Typing without an editable focus makes macOS beep on every character: refuse instead.
            let root = AXUIElementCreateApplication(ra.processIdentifier)
            let focused = attr(root, kAXFocusedUIElementAttribute).map { $0 as! AXUIElement }
            let role = focused.map { str($0, kAXRoleAttribute) } ?? ""
            let editable = ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"].contains(role)
                || (focused.flatMap { attr($0, "AXEditableAncestor") } != nil)
            // Keyboard-driven apps take typed keys without a text field (Calculator: digits/operators).
            let keyboardApps: Set<String> = ["com.apple.calculator"]
            let charsOk = keyboardApps.contains(ra.bundleIdentifier ?? "") && t.allSatisfy { "0123456789.,+-*/=()%^xX ".contains($0) }
            guard editable || charsOk else { stats["refused_no_text_focus", default: 0] += 1; return ["error": "no text field is focused (focused: \(role.isEmpty ? "nothing" : role)); focus a field first"] }
            typeText(t, pid: ra.processIdentifier); stats["text_sent", default: 0] += 1; return ["ok": true]
        }
        stats["keys_sent", default: 0] += 1
        return combo(body["combo"] as? String ?? "", pid: ra.processIdentifier)
    case ("POST", "/dismiss"):
        // Close an open sheet/dialog via its Cancel button (AX) — no Escape key, no beep.
        guard let ra = app(named: body["app"] as? String ?? "") else { return ["error": "app not running"] }
        guard let sh = sheets(ra).first else { return ["ok": true, "note": "no dialog open"] }
        let cancelWords = ["abbrechen", "cancel", "schließen", "close"]
        let b: AXUIElement? = attr(sh, kAXCancelButtonAttribute).map { $0 as! AXUIElement } ?? findAll(sh, role: "AXButton").first { cancelWords.contains(str($0, kAXTitleAttribute).lowercased()) }
        guard let cancel = b else { return ["error": "dialog has no cancel button"] }
        AXUIElementPerformAction(cancel, kAXPressAction as CFString); stats["dismissed", default: 0] += 1
        usleep(300000)
        return ["ok": true]
    case ("GET", "/stats"): return stats
    case ("POST", "/screenshot"): return await screenshot(body["app"] as? String ?? "", maxWidth: body["max_width"] as? Int ?? 1024, anyLayer: body["any_layer"] as? Bool ?? false, largest: body["largest"] as? Bool ?? false, title: body["title"] as? String)
    case ("POST", "/click"): return click(body["x"] as? Double ?? 0, body["y"] as? Double ?? 0, double: body["double"] as? Bool ?? false, app: body["app"] as? String ?? "")
    case ("POST", "/activate"): return openApp(body["app"] as? String ?? "")
    case ("POST", "/open_file"):
        let path = body["path"] as? String ?? ""
        guard FileManager.default.fileExists(atPath: path) else { return ["error": "no such file: \(path)"] }
        if let a = body["app"] as? String, !a.isEmpty { return runOpen(a.contains(".") && !a.contains(" ") ? ["-b", a, path] : ["-a", a, path]) }
        return runOpen([path])
    case ("POST", "/open_url"):
        let u = body["url"] as? String ?? ""
        guard let scheme = URL(string: u)?.scheme, ["http", "https", "x-apple.systempreferences", "mailto"].contains(scheme) else { return ["error": "url scheme not allowed"] }
        return runOpen([u])
    case ("POST", "/save_as"): return saveAs(body["app"] as? String ?? "", body["path"] as? String ?? "")
    case ("POST", "/window_at"):
        return windowAt(x: body["x"] as? Double ?? 0, y: body["y"] as? Double ?? 0, maxChars: body["max_chars"] as? Int ?? 12_000)
    case ("POST", "/window_read"):
        // Read a specific window by app + title, without raising it (background read).
        guard let ra = app(named: body["app"] as? String ?? "") else { return ["error": "app not running"] }
        let wins = (attr(AXUIElementCreateApplication(ra.processIdentifier), kAXWindowsAttribute) as? [AXUIElement]) ?? []
        let t = body["title"] as? String ?? ""
        guard let w = wins.first(where: { t.isEmpty || str($0, kAXTitleAttribute).localizedCaseInsensitiveContains(t) }) else { return ["error": "window not found"] }
        return readWindowText(w, pid: ra.processIdentifier, maxChars: body["max_chars"] as? Int ?? 12_000)
    case ("POST", "/move_window"):
        // Move one window (AX position, top-left coords) — e.g. onto Sam's own display.
        guard let ra = app(named: body["app"] as? String ?? "") else { return ["error": "app not running"] }
        let wins = (attr(AXUIElementCreateApplication(ra.processIdentifier), kAXWindowsAttribute) as? [AXUIElement]) ?? []
        let t = body["title"] as? String ?? ""
        guard let w = wins.first(where: { t.isEmpty || str($0, kAXTitleAttribute).localizedCaseInsensitiveContains(t) }) else { return ["error": "window not found"] }
        var pt = CGPoint(x: body["x"] as? Double ?? 0, y: body["y"] as? Double ?? 0)
        guard let v = AXValueCreate(.cgPoint, &pt) else { return ["error": "value"] }
        let e = AXUIElementSetAttributeValue(w, kAXPositionAttribute as CFString, v)
        return e == .success ? ["ok": true, "frame": frame(w).map { [$0.minX, $0.minY, $0.width, $0.height] } ?? []] : ["error": "AX error \(e.rawValue)"]
    case ("POST", "/raise"):
        // Bring one specific window of an app to the front (tests point only at their own window).
        guard let ra = app(named: body["app"] as? String ?? "") else { return ["error": "app not running"] }
        let root = AXUIElementCreateApplication(ra.processIdentifier)
        let t = body["title"] as? String ?? ""
        let wins = (attr(root, kAXWindowsAttribute) as? [AXUIElement]) ?? []
        guard let w = wins.first(where: { t.isEmpty ? true : str($0, kAXTitleAttribute).localizedCaseInsensitiveContains(t) }) else { return ["error": "window not found"] }
        _ = runOpen(ra.bundleIdentifier.map { ["-b", $0] } ?? [])
        AXUIElementPerformAction(w, kAXRaiseAction as CFString)
        AXUIElementSetAttributeValue(w, kAXMainAttribute as CFString, kCFBooleanTrue)
        usleep(300000)
        return ["ok": true]
    case ("POST", "/window_frame"): return windowFrame(body["app"] as? String ?? "", title: body["title"] as? String)
    case ("POST", "/make_doc"):
        return await makeDoc(body["html"] as? String ?? "", body["out"] as? String ?? "", body["format"] as? String ?? "rtfd")
    case ("POST", "/new_document"):
        // New document through the app's scripting dictionary (skips iCloud "Open" browsers).
        guard let ra = app(named: body["app"] as? String ?? ""), let bid = ra.bundleIdentifier else { return ["error": "app not running"] }
        let t = (body["text"] as? String ?? "").replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
        var err: NSDictionary?
        NSAppleScript(source: "with timeout of 15 seconds\ntell application id \"\(bid)\"\nmake new document with properties {text:\"\(t)\"}\nactivate\nend tell\nend timeout")?.executeAndReturnError(&err)
        if let e = err { return ["error": "new document failed: \(e[NSAppleScript.errorMessage] ?? e)"] }
        usleep(400000)
        return ["ok": true]
    case ("POST", "/save"):
        // Save in place: the document already has a file the app may write (sandbox-safe).
        guard let ra = app(named: body["app"] as? String ?? ""), let bid = ra.bundleIdentifier else { return ["error": "app not running"] }
        var err: NSDictionary?
        NSAppleScript(source: "with timeout of 15 seconds\ntell application id \"\(bid)\"\nif (path of front document) is missing value then error \"document has no file yet; use save_as\"\nsave front document\nend tell\nend timeout")?.executeAndReturnError(&err)
        if let e = err { return ["error": "save failed: \(e[NSAppleScript.errorMessage] ?? e)"] }
        return ["ok": true]
    case ("POST", "/save_dialog"):
        let ie = body["if_exists"] as? String ?? ((body["overwrite"] as? Bool) == true ? "overwrite" : "fail")
        return saveDialog(body["app"] as? String ?? "", body["path"] as? String ?? "", ifExists: ie, minBytes: Int64(body["min_bytes"] as? Int ?? 1_048_576), skipPreflight: body["skip_preflight"] as? Bool ?? false)
    case ("GET", "/front"): return ["app": NSWorkspace.shared.frontmostApplication?.localizedName ?? ""]
    // Suggestions (server suggest.rs): Apple Maps points of interest near a place, and geocoding.
    // Read-only, Apple's own data, no account; names, categories, addresses, phones, websites
    // (no opening hours or ratings on macOS: those come from OpenStreetMap).
    case ("POST", "/places"): return await places(body["query"] as? String ?? "", lat: body["lat"] as? Double ?? 0, lon: body["lon"] as? Double ?? 0, radius: body["radius_m"] as? Double ?? 1500, max: body["max"] as? Int ?? 20)
    case ("POST", "/geocode"): return await geocode(body["place"] as? String ?? "")
    // The keyboard's media key (play/pause, next, previous), system-wide like the real key: the
    // browser tab or player Sam started answers it. No app is operated, no text is typed.
    case ("POST", "/media"): return mediaKey(body["key"] as? String ?? "playpause")
    // YouTube in the user's default browser: open the page and press the player's own Play
    // button when it didn't autoplay (browsers hold back sound until a gesture); pause / next /
    // previous through the same buttons. Only the player's buttons are ever pressed.
    case ("POST", "/youtube"): return youtube(url: body["url"] as? String ?? "", action: body["action"] as? String ?? "play")
    default: return ["error": "not found"]
    }
}

func places(_ query: String, lat: Double, lon: Double, radius: Double, max: Int) async -> [String: Any] {
    guard !query.isEmpty, lat != 0 else { return ["error": "query, lat and lon needed"] }
    let req = MKLocalSearch.Request()
    req.naturalLanguageQuery = query
    req.region = MKCoordinateRegion(center: CLLocationCoordinate2D(latitude: lat, longitude: lon), latitudinalMeters: radius * 2, longitudinalMeters: radius * 2)
    req.resultTypes = [.pointOfInterest]
    let here = CLLocation(latitude: lat, longitude: lon)
    do {
        let resp = try await MKLocalSearch(request: req).start()
        let items: [[String: Any]] = resp.mapItems.prefix(max).map { it in
            let c = it.placemark.coordinate
            return ["name": it.name ?? "", "category": (it.pointOfInterestCategory?.rawValue ?? "").replacingOccurrences(of: "MKPOICategory", with: ""),
                    "address": "\(it.placemark.thoroughfare ?? "") \(it.placemark.subThoroughfare ?? "")".trimmingCharacters(in: .whitespaces), "postcode": it.placemark.postalCode ?? "",
                    "phone": it.phoneNumber ?? "", "url": it.url?.absoluteString ?? "", "lat": c.latitude, "lon": c.longitude,
                    "distance_m": Int(CLLocation(latitude: c.latitude, longitude: c.longitude).distance(from: here))]
        }
        return ["places": items]
    } catch { return ["error": error.localizedDescription] }
}

func mediaKey(_ key: String) -> [String: Any] {
    let code: Int32
    switch key {
    case "playpause", "play", "pause": code = 16   // NX_KEYTYPE_PLAY
    case "next": code = 17                         // NX_KEYTYPE_NEXT
    case "previous", "prev": code = 18             // NX_KEYTYPE_PREVIOUS
    default: return ["error": "unknown media key \(key)"]
    }
    func post(_ down: Bool) -> Bool {
        let flags = NSEvent.ModifierFlags(rawValue: down ? 0xa00 : 0xb00)
        let data1 = Int((code << 16) | ((down ? 0xa : 0xb) << 8))
        guard let ev = NSEvent.otherEvent(with: .systemDefined, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil, subtype: 8, data1: data1, data2: -1) else { return false }
        ev.cgEvent?.post(tap: .cghidEventTap)
        return true
    }
    guard post(true), post(false) else { return ["error": "media key could not be posted"] }
    stats["media_keys", default: 0] += 1
    return ["ok": true, "key": key]
}

// MARK: - YouTube player (Accessibility)

let browserBundles: Set<String> = ["org.mozilla.firefox", "com.google.chrome", "com.apple.safari", "com.brave.browser", "com.microsoft.edgemac", "company.thebrowser.browser", "com.vivaldi.vivaldi", "com.operasoftware.opera"]

/// The player's button whose accessible name ends with `suffix` ("(k)" = play/pause,
/// "(SHIFT+n)" = next, "(SHIFT+p)" = previous) in one of the browser's windows: the focused
/// window first, then any window titled with YouTube. Returns the element and its name.
/// The player's buttons are known by their keyboard shortcut, whatever the page language and
/// however the browser composes the name: "Play (k)", "Wiedergeben (k)", "Pause Tastenkombination k"
/// (Firefox appends aria-keyshortcuts), "Next (SHIFT+n)" / "Weiter (SHIFT+n)".
func ytNamed(_ label: String, _ key: String) -> Bool {
    let l = label.lowercased()
    if key == "(k)" {
        let last = l.split(separator: " ").last.map { $0.trimmingCharacters(in: CharacterSet(charactersIn: "()")) } ?? ""
        return last == "k" && !l.contains("shift")
    }
    return l.contains(key.lowercased())
}

func ytButton(_ ra: NSRunningApplication, suffix: String) -> (AXUIElement, String)? {
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    var wins: [AXUIElement] = []
    if let w = attr(root, kAXFocusedWindowAttribute) { wins.append(w as! AXUIElement) }
    for w in (attr(root, kAXWindowsAttribute) as? [AXUIElement]) ?? [] where str(w, kAXTitleAttribute).localizedCaseInsensitiveContains("youtube") { wins.append(w) }
    // Several buttons carry the same name (the page's hidden copies measure 0×0): the visible
    // one with the largest frame is the player's.
    var found: [(AXUIElement, String, CGFloat)] = []
    var visited = 0
    func walk(_ e: AXUIElement, depth: Int) {
        if visited > 12_000 || depth > 80 { return }
        visited += 1
        if str(e, kAXRoleAttribute) == "AXButton" {
            for label in [str(e, kAXDescriptionAttribute), str(e, kAXTitleAttribute), str(e, kAXHelpAttribute)] where ytNamed(label, suffix) {
                let f = frame(e) ?? .zero
                found.append((e, label, f.width * f.height)); break
            }
        }
        if let kids = attr(e, kAXChildrenAttribute) as? [AXUIElement] { for k in kids { walk(k, depth: depth + 1) } }
    }
    for w in wins { walk(w, depth: 0); if !found.isEmpty { break } }
    return found.max { $0.2 < $1.2 }.map { ($0.0, $0.1) }
}

/// A browser exposes only its current tab's page: when another tab is showing, the tab whose
/// title names YouTube is pressed (the tab strip's own button) so the player becomes reachable.
/// Returns the tab's title when one was switched to.
func ytSwitchTab(_ ra: NSRunningApplication) -> String? {
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    var wins: [AXUIElement] = []
    if let w = attr(root, kAXFocusedWindowAttribute) { wins.append(w as! AXUIElement) }
    for w in (attr(root, kAXWindowsAttribute) as? [AXUIElement]) ?? [] where !wins.contains(where: { CFEqual($0, w) }) { wins.append(w) }
    var tab: (AXUIElement, String)? = nil
    var visited = 0
    func walk(_ e: AXUIElement, depth: Int) {
        if tab != nil || visited > 3000 || depth > 30 { return }
        visited += 1
        let role = str(e, kAXRoleAttribute)
        if role == "AXRadioButton" || role == "AXTab" || str(e, kAXSubroleAttribute) == "AXTabButton" {
            let t = [str(e, kAXTitleAttribute), str(e, kAXDescriptionAttribute)].first { !$0.isEmpty } ?? ""
            if t.localizedCaseInsensitiveContains("youtube") { tab = (e, t); return }
        }
        // Tabs live in the window's toolbar area: web content (AXWebArea) is skipped.
        if role == "AXWebArea" { return }
        if let kids = attr(e, kAXChildrenAttribute) as? [AXUIElement] { for k in kids { walk(k, depth: depth + 1) } }
    }
    for w in wins { visited = 0; walk(w, depth: 0); if tab != nil { break } }
    guard let (el, title) = tab else { return nil }
    guard AXUIElementPerformAction(el, kAXPressAction as CFString) == .success else { return nil }
    usleep(1_200_000)
    return title
}

func youtube(url: String, action: String) -> [String: Any] {
    var ra: NSRunningApplication? = nil
    var opened = false
    var switched = false
    if !url.isEmpty {
        guard let u = URL(string: url), u.scheme == "https", (u.host ?? "").hasSuffix("youtube.com") else { return ["error": "a youtube.com url is needed"] }
        guard let appURL = NSWorkspace.shared.urlForApplication(toOpen: u), let bundle = Bundle(url: appURL)?.bundleIdentifier else { return ["error": "no default browser"] }
        NSWorkspace.shared.open(u)
        opened = true
        for _ in 0..<40 { if let a = app(named: bundle) { ra = a; break }; usleep(250_000) }
    } else {
        // The browser showing YouTube: the frontmost one first, then any running browser.
        var cands = NSWorkspace.shared.runningApplications.filter { browserBundles.contains(($0.bundleIdentifier ?? "").lowercased()) }
        if let f = NSWorkspace.shared.frontmostApplication, let i = cands.firstIndex(where: { $0.processIdentifier == f.processIdentifier }) { cands.swapAt(0, i) }
        // A browser builds (or re-enables) its web Accessibility tree only once a client asks:
        // a few seconds of polling after the first query.
        for c in cands {
            let r = AXUIElementCreateApplication(c.processIdentifier)
            _ = AXUIElementSetAttributeValue(r, "AXManualAccessibility" as CFString, kCFBooleanTrue)
            _ = AXUIElementSetAttributeValue(r, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)
            _ = attr(r, kAXFocusedUIElementAttribute)
        }
        for i in 0..<16 {
            ra = cands.first { ytButton($0, suffix: "(k)") != nil }
            if ra != nil { break }
            // After the tree had time to build: the YouTube tab may simply not be the current one.
            if i == 3, let c = cands.first(where: { ytSwitchTab($0) != nil }) { ra = c; switched = true; break }
            usleep(500_000)
        }
    }
    guard let ra = ra else { return ["error": url.isEmpty ? "no browser window with a YouTube player" : "the browser didn't come up", "opened": opened] }
    // Chromium builds its web Accessibility tree only once a client asks for it (Firefox and
    // Safari expose it as they are).
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    _ = AXUIElementSetAttributeValue(root, "AXManualAccessibility" as CFString, kCFBooleanTrue)
    _ = AXUIElementSetAttributeValue(root, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)
    let suffix = action == "next" ? "(SHIFT+n)" : action == "previous" ? "(SHIFT+p)" : "(k)"
    var hit: (AXUIElement, String)? = nil
    // Up to ~15 s for the page (and the player) to appear after opening.
    for _ in 0..<30 { hit = ytButton(ra, suffix: suffix); if hit != nil { break }; usleep(500_000) }
    guard let (btn, label) = hit else { return ["error": "the YouTube player isn't reachable in \(ra.localizedName ?? "the browser")", "opened": opened, "browser": ra.localizedName ?? ""] }
    let playing = { (l: String) in l.lowercased().hasPrefix("pau") }
    var pressed = false
    var err: AXError = .success
    let acts = actions(btn)
    // Firefox's web buttons carry AXPress; the web area's own action name is tried too.
    func press() -> Bool {
        for a in [kAXPressAction, "AXPress", "AXConfirm"] {
            err = AXUIElementPerformAction(btn, a as CFString)
            if err == .success { return true }
        }
        return false
    }
    let want = (action == "play" || action == "resume") ? !playing(label) : (action == "pause" || action == "stop") ? playing(label) : true
    if want { pressed = press() }
    if pressed { usleep(900_000) }
    let after = ytButton(ra, suffix: "(k)")?.1 ?? label
    stats["youtube_presses", default: 0] += pressed ? 1 : 0
    // The page title without the browser's and YouTube's suffixes, for "paused: <title>".
    let win = attr(AXUIElementCreateApplication(ra.processIdentifier), kAXFocusedWindowAttribute).map { str($0 as! AXUIElement, kAXTitleAttribute) } ?? ""
    let title = win.components(separatedBy: " - YouTube").first.map { String($0) } ?? win
    _ = (acts, err)
    return ["ok": true, "playing": playing(after), "pressed": pressed, "label": after, "browser": ra.localizedName ?? "", "opened": opened, "switched": switched, "title": title]
}

func geocode(_ place: String) async -> [String: Any] {
    guard !place.isEmpty else { return ["error": "place needed"] }
    do {
        let marks = try await CLGeocoder().geocodeAddressString(place)
        guard let m = marks.first, let loc = m.location else { return ["error": "not found"] }
        return ["lat": loc.coordinate.latitude, "lon": loc.coordinate.longitude, "name": [m.subLocality, m.locality, m.country].compactMap { $0 }.joined(separator: ", ")]
    } catch { return ["error": error.localizedDescription] }
}

// Local access control (same rules as the Rust server, localauth.rs): Host must be localhost
// (no DNS rebinding), no browser Origin at all (websites can't post here, not even text/plain
// "simple" requests), and the caller presents the local token from a 0600 file.
enum LocalAuth {
    static let path = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/Sam/local-token")
    static let token: String = {
        if let t = try? String(contentsOf: path, encoding: .utf8), t.trimmingCharacters(in: .whitespacesAndNewlines).count >= 32 { return t.trimmingCharacters(in: .whitespacesAndNewlines) }
        try? FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
        var bytes = [UInt8](repeating: 0, count: 32); _ = SecRandomCopyBytes(kSecRandomDefault, 32, &bytes)
        let t = bytes.map { String(format: "%02x", $0) }.joined()
        let fd = open(path.path, O_WRONLY | O_CREAT | O_EXCL, 0o600)
        if fd >= 0 { _ = t.withCString { write(fd, $0, strlen($0)) }; close(fd); return t }
        return (try? String(contentsOf: path, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines) ?? t
    }()
}

/// Read-only / placement endpoints; everything that types, clicks, opens or saves is computer use
/// and only works when the user enabled it (flag file or SAM_COMPUTER_USE=1).
// Native chat apps (WhatsApp…): read-only snapshots and a send that can only press the app's own
// Send button over the exact approved text. On unless the user turned native chats off.
let chatApps: Set<String> = ["net.whatsapp.WhatsApp", "ru.keepcoder.Telegram", "com.tinyspeck.slackmacgap"]
/// Electron apps (Slack) build their Accessibility tree only once a client asks for it.
let electronApps: Set<String> = ["com.tinyspeck.slackmacgap"]
func enableElectronAX(_ bundle: String) {
    guard electronApps.contains(bundle), let ra = app(named: bundle) else { return }
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    let r1 = AXUIElementSetAttributeValue(root, "AXManualAccessibility" as CFString, kCFBooleanTrue)
    let r2 = AXUIElementSetAttributeValue(root, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)
    _ = (r1, r2)
    usleep(800_000)
    // Asking for the focused element / navigation order makes Chromium build its web tree.
    _ = attr(root, kAXFocusedUIElementAttribute)
    if let w = attr(root, kAXFocusedWindowAttribute) ?? (attr(root, kAXWindowsAttribute) as? [AXUIElement])?.first {
        _ = attr(w as! AXUIElement, "AXChildrenInNavigationOrder")
    }
    usleep(1_200_000)
}
var nativeChatsEnabled: Bool { !FileManager.default.fileExists(atPath: LocalAuth.path.deletingLastPathComponent().appendingPathComponent("native-chats-disabled").path) }

func chatRead(_ bundle: String) -> [String: Any] {
    guard chatApps.contains(bundle) else { return ["error": "not a chat app"] }
    guard nativeChatsEnabled else { return ["error": "native chat apps are off"] }
    enableElectronAX(bundle)
    var r = snapshot(bundle, max: 400, menus: false)
    lock.lock(); snap.elements = [:]; lock.unlock()   // read-only: no element ids to act on
    r["frontmost"] = NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? ""
    return r
}

/// Presses Send in a chat app only if its message box holds exactly `expect`; verifies the box
/// emptied afterwards. Never types, never clicks anything else.
func chatSend(_ bundle: String, expect: String) -> [String: Any] {
    guard chatApps.contains(bundle) else { return ["error": "not a chat app"] }
    guard nativeChatsEnabled else { return ["error": "native chat apps are off"] }
    enableElectronAX(bundle)
    guard let ra = app(named: bundle) else { return ["error": "app not running"] }
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    func norm(_ t: String) -> String { t.replacingOccurrences(of: "\u{200E}", with: "").trimmingCharacters(in: .whitespacesAndNewlines) }
    func composer() -> AXUIElement? { findAll(root, role: "AXTextArea", depth: 0).last }
    guard let box = composer() else { return ["error": "no message box"] }
    let have = norm(str(box, kAXValueAttribute))
    guard !expect.isEmpty, have == norm(expect) else { return ["error": "the message box doesn't hold the approved text", "box": String(have.prefix(200))] }
    let sendLabels: Set<String> = ["senden", "send", "envoyer", "enviar", "invia"]
    let buttons = findAll(root, role: "AXButton", depth: 0)
    guard let send = buttons.first(where: { b in [str(b, kAXTitleAttribute), str(b, kAXDescriptionAttribute)].contains { sendLabels.contains(norm($0).lowercased()) } }) else {
        return ["error": "no Send button"]
    }
    let err = AXUIElementPerformAction(send, kAXPressAction as CFString)
    guard err == .success else { return ["error": "press failed (\(err.rawValue))"] }
    for _ in 0..<20 {
        usleep(100000)
        if let b = composer(), norm(str(b, kAXValueAttribute)).isEmpty { return ["ok": true, "sent": true] }
    }
    return ["error": "pressed Send but the message box didn't empty"]
}

/// Empties a chat app's message box only if it holds exactly Sam's draft `expect` (a cancelled
/// draft; whatever the user typed stays).
func chatClear(_ bundle: String, expect: String) -> [String: Any] {
    guard chatApps.contains(bundle) else { return ["error": "not a chat app"] }
    guard nativeChatsEnabled else { return ["error": "native chat apps are off"] }
    enableElectronAX(bundle)
    guard let ra = app(named: bundle) else { return ["error": "app not running"] }
    let root = AXUIElementCreateApplication(ra.processIdentifier)
    func norm(_ t: String) -> String { t.replacingOccurrences(of: "\u{200E}", with: "").trimmingCharacters(in: .whitespacesAndNewlines) }
    guard let box = findAll(root, role: "AXTextArea", depth: 0).last else { return ["error": "no message box"] }
    guard !expect.isEmpty, norm(str(box, kAXValueAttribute)) == norm(expect) else { return ["error": "the message box doesn't hold Sam's draft"] }
    let err = AXUIElementSetAttributeValue(box, kAXValueAttribute as CFString, "" as CFString)
    return err == .success ? ["ok": true] : ["error": "clear failed (\(err.rawValue))"]
}

let safeRoutes: Set<String> = ["/chat_read", "/chat_send", "/chat_clear", "/health", "/request_access", "/window_at", "/window_read", "/window_frame", "/front", "/screenshot", "/stats", "/move_window", "/places", "/geocode", "/media", "/youtube"]
var computerUseEnabled: Bool {
    ProcessInfo.processInfo.environment["SAM_COMPUTER_USE"] == "1"
        || FileManager.default.fileExists(atPath: LocalAuth.path.deletingLastPathComponent().appendingPathComponent("computer-use-enabled").path)
}

func authorize(_ head: [Substring], path: String) -> String? {
    var h: [String: String] = [:]
    for l in head.dropFirst() { if let i = l.firstIndex(of: ":") { h[l[..<i].lowercased()] = l[l.index(after: i)...].trimmingCharacters(in: .whitespaces) } }
    guard h["host"] == "127.0.0.1:47803" || h["host"] == "localhost:47803" else { return "bad host" }
    guard h["origin"] == nil else { return "browser origin refused" }
    guard let t = h["x-sam-token"], t == LocalAuth.token else { return "missing token" }
    let route = String(path.split(separator: "?").first ?? "")
    if !safeRoutes.contains(route) && !computerUseEnabled { return "computer use is off (enable it in Sam first)" }
    return nil
}

let params = NWParameters.tcp
params.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: 47803)  // never reachable from the network
let listener = try! NWListener(using: params)
listener.newConnectionHandler = { conn in
    conn.start(queue: .global())
    var buf = Data()
    func receive() {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { data, _, done, _ in
            if let d = data { buf.append(d) }
            if let r = buf.range(of: Data("\r\n\r\n".utf8)) {
                let head = String(decoding: buf[..<r.lowerBound], as: UTF8.self)
                let lines = head.split(separator: "\r\n")
                let req = lines.first?.split(separator: " ") ?? []
                let len = lines.first { $0.lowercased().hasPrefix("content-length:") }.flatMap { Int($0.split(separator: ":")[1].trimmingCharacters(in: .whitespaces)) } ?? 0
                let bodyData = buf[r.upperBound...]
                if bodyData.count >= len {
                    let body = (try? JSONSerialization.jsonObject(with: Data(bodyData.prefix(len)))) as? [String: Any] ?? [:]
                    let path = req.count > 1 ? String(req[1]) : ""
                    if let why = authorize(lines, path: path) {
                        let out = Data("{\"error\":\"forbidden: \(why)\"}".utf8)
                        let hdr = "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: \(out.count)\r\nConnection: close\r\n\r\n"
                        conn.send(content: Data(hdr.utf8) + out, completion: .contentProcessed { _ in conn.cancel() })
                        return
                    }
                    Task {
                        let res = await handle(req.count > 0 ? String(req[0]) : "", path, body)
                        let out = (try? JSONSerialization.data(withJSONObject: res)) ?? Data("{}".utf8)
                        let hdr = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \(out.count)\r\nConnection: close\r\n\r\n"
                        conn.send(content: Data(hdr.utf8) + out, completion: .contentProcessed { _ in conn.cancel() })
                    }
                    return
                }
            }
            if done { conn.cancel() } else { receive() }
        }
    }
    receive()
}
listener.start(queue: .main)
print("sam computer-use on 127.0.0.1:47803 ax=\(AXIsProcessTrusted()) screen=\(CGPreflightScreenCaptureAccess())")
NSApplication.shared.setActivationPolicy(.accessory)
NSApplication.shared.run()
