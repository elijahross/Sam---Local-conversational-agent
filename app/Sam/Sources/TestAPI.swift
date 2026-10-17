// E2E control surface (only with SAM_TEST=1 / --test, localhost only): drive activation,
// inject speech WAVs instead of the mic, read state + timeline.
import AppKit
import ApplicationServices
import Network
import SwiftUI

@MainActor
final class TestAPI {
    let c: Controller
    private var listener: NWListener?

    init(controller: Controller) {
        c = controller
        let p = NWParameters.tcp
        p.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: 47804)
        listener = try? NWListener(using: p)
        listener?.newConnectionHandler = { [weak self] conn in self?.serve(conn) }
        listener?.start(queue: .main)
    }

    private nonisolated func serve(_ conn: NWConnection) {
        conn.start(queue: .main)
        var buf = Data()
        func rx() {
            conn.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { d, _, done, _ in
                if let d { buf.append(d) }
                if let r = buf.range(of: Data("\r\n\r\n".utf8)) {
                    let head = String(decoding: buf[..<r.lowerBound], as: UTF8.self)
                    let lines = head.split(separator: "\r\n")
                    let req = lines.first?.split(separator: " ") ?? []
                    let len = lines.first { $0.lowercased().hasPrefix("content-length:") }.flatMap { Int($0.split(separator: ":")[1].trimmingCharacters(in: .whitespaces)) } ?? 0
                    if buf[r.upperBound...].count >= len {
                        let body = (try? JSONSerialization.jsonObject(with: Data(buf[r.upperBound...].prefix(len)))) as? [String: Any] ?? [:]
                        let path = req.count > 1 ? String(req[1]) : "/"
                        guard LocalAuth.allowed(lines, port: 47804) else {
                            conn.send(content: Data("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".utf8), completion: .contentProcessed { _ in conn.cancel() })
                            return
                        }
                        Task { @MainActor in
                            let res = await self.handle(path, body)
                            let out = (try? JSONSerialization.data(withJSONObject: res)) ?? Data("{}".utf8)
                            conn.send(content: Data("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \(out.count)\r\nConnection: close\r\n\r\n".utf8) + out,
                                      completion: .contentProcessed { _ in conn.cancel() })
                        }
                        return
                    }
                }
                if done { conn.cancel() } else { rx() }
            }
        }
        rx()
    }

    private func handle(_ path: String, _ b: [String: Any]) async -> [String: Any] {
        let m = c.model
        switch path {
        case "/state":
            let r = c.notch.islandScreenRect()
            return ["state": m.state.rawValue, "connected": m.connected, "transcript": m.transcript, "answer": m.answer,
                    "cards": m.cards.map { ["id": $0.id, "kind": $0.kind, "action_id": ($0.confirm?["action_id"] as? String) ?? "", "open": $0.open ?? ""] },
                    "context_chip": m.contextChip ?? "",
                    "cards_full": m.cards.map { ["id": $0.id, "kind": $0.kind, "blocks": $0.blocks.map { $0.raw }, "confirm": $0.confirm ?? [:]] as [String: Any] }, "error": m.error ?? "", "hands_free": m.handsFree,
                    "mic_running": c.audio.micRunning, "voice_off": m.voiceOff, "engine_running": c.audio.engineRunning, "playing": c.audio.isPlaying,
                    "panel_key": c.notch.panel.isKeyWindow, "voice": m.voiceChoice, "frontmost": NSWorkspace.shared.frontmostApplication?.localizedName ?? "",
                    "island_rect": [r.minX, r.minY, r.width, r.height], "panel_height": m.panelHeight, "ptt_held": m.pttHeld,
                    "log": m.log.map { ["e": $0.0, "t": Int($0.1)] }]
        case "/trigger":
            // Every trigger sets or clears the point: a stale point must never carry into another test.
            c.testPoint = (b["point"] as? [Double]).flatMap { $0.count == 2 ? CGPoint(x: $0[0], y: $0[1]) : nil }
            switch b["action"] as? String {
            case "hover_enter": c.hover(true)
            case "hover_exit": c.hover(false)
            case "ptt_down": c.ptt(true)
            case "ptt_up": c.ptt(false)
            // The real fn logic (gating, taps, combos) without a keyboard.
            case "fn_down": c.keys.fn(true)
            case "fn_up": c.keys.fn(false)
            case "fn_combo_key": c.keys.otherKey()
            case "toggle": c.toggle()
            case "close": c.close()
            case "dictate_down": c.testDictationBundle = b["target"] as? String; c.dictate(true)
            case "dictate_up": c.dictate(false)
            default: return ["error": "unknown action"]
            }
            return ["ok": true]
        case "/inject":
            guard let p = b["wav"] as? String else { return ["error": "wav"] }
            m.mark("inject_start")
            c.injectWav(p) { Task { @MainActor in m.mark("inject_end") } }
            return ["ok": true]
        case "/confirm":
            c.notch.onConfirm?(b["action_id"] as? String ?? "", b["decision"] as? String ?? "approve", b["args"] as? [String: Any] ?? [:])
            return ["ok": true]
        case "/voice":   // the character menu
            guard let v = b["voice"] as? String, ["sam-male", "sam-female"].contains(v) else { return ["error": "voice"] }
            m.voiceChoice = v
            return ["ok": true]
        case "/ui":   // menu-bar drop-down and task overview
            switch b["action"] as? String {
            case "open_overview":
                // Tests pass a point on Sam's virtual display so the backdrop never covers the user's screen.
                let scr = (b["screen_point"] as? [Double]).flatMap { p in NSScreen.screens.first { $0.frame.contains(CGPoint(x: p[0], y: p[1])) } }
                c.overview.show(task: b["task"] as? String, screen: scr)
            case "close_overview": c.overview.hide()
            case "overview_tab": c.overview.layout.tab = OverviewTab(rawValue: b["tab"] as? String ?? "") ?? .recent
            case "you_section": c.overview.layout.youSection = YouSection(rawValue: b["section"] as? String ?? "")
            case "checkin_now": c.overview.friend.checkInNow()
            case "history_query":   // tests: drive the History filters like a user would
                let h = c.overview.history
                h.query = b["q"] as? String ?? ""
                h.status = b["status"] as? String ?? ""
                h.range = HistoryRange(rawValue: b["range"] as? String ?? "") ?? .all
                h.sort = b["sort"] as? String ?? ""
                await h.search()
            case "open_dropdown": c.statusBar.show(toast: b["toast"] as? String)
            case "close_dropdown": c.statusBar.hide()
            case "open_connectors":
                let scr = (b["screen_point"] as? [Double]).flatMap { p in NSScreen.screens.first { $0.frame.contains(CGPoint(x: p[0], y: p[1])) } }
                c.connectors.show(screen: scr)
            case "close_connectors": c.connectors.hide()
            case "open_onboarding":
                let scr = (b["screen_point"] as? [Double]).flatMap { p in NSScreen.screens.first { $0.frame.contains(CGPoint(x: p[0], y: p[1])) } }
                c.onboarding.show(step: b["step"] as? Int ?? 0, screen: scr)
            case "onboarding_step": c.onboarding.model.step = b["step"] as? Int ?? 0
            case "onboarding_finish": c.onboarding.model.onFinish()
            case "onboarding_snapshot":   // tests: render the onboarding view to a PNG in-process (no screen recording needed)
                guard let path = b["path"] as? String else { return ["error": "path needed"] }
                let renderer = ImageRenderer(content: OnboardingView(m: c.onboarding.model).frame(width: 920, height: 640))
                renderer.scale = 2
                guard let img = renderer.nsImage, let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff), let png = rep.representation(using: .png, properties: [:]) else { return ["error": "render failed"] }
                try? png.write(to: URL(fileURLWithPath: path))
                return ["ok": true, "w": Int(img.size.width), "h": Int(img.size.height)]
            case "toasts": c.toasts = b["on"] as? Bool ?? false
            case "announce": c.announceInTests = b["on"] as? Bool ?? false
            case "voice_off": c.model.voiceOff = b["on"] as? Bool ?? false
            case "uninstall_plan": return ["plan": Uninstaller.plan()]   // what an uninstall would remove; never acts
            case "handsfree_idle":   // tests: hands-free close timeouts (seconds)
                c.idleFirst = b["first"] as? Double ?? 60; c.idleFollowUp = b["follow"] as? Double ?? 60
            case "select": c.tasks.selected = b["task"] as? String
            case "delete_task": if let id = b["task"] as? String { c.tasks.delete(id) }
            case "render_report":   // tests: the whole report as one image (maps don't render offscreen)
                guard let rep = c.tasks.detail?.report, let out = b["path"] as? String else { return ["error": "no report selected"] }
                let view = ReportView(r: rep, onOpen: { _ in }).padding(28).frame(width: 820).background(Color(white: 0.11)).environment(\.colorScheme, .dark)
                let r = ImageRenderer(content: view)
                r.scale = 1
                try? await Task.sleep(for: .seconds(3))   // pictures load async
                guard let img = r.nsImage, let tiff = img.tiffRepresentation, let rep2 = NSBitmapImageRep(data: tiff),
                      let png = rep2.representation(using: .png, properties: [:]) else { return ["error": "render failed"] }
                try? png.write(to: URL(fileURLWithPath: out))
            default: return ["error": "unknown ui action"]
            }
            return ["ok": true]
        case "/ui_state":
            await c.tasks.refresh()
            return ["overview_open": c.overview.isOpen, "dropdown_open": c.statusBar.isOpen, "toast": c.statusBar.dmodel.toast ?? "",
                    "status_title": c.statusBar.item.button?.title ?? "", "tasks": c.tasks.tasks.count, "running": c.tasks.running,
                    "needs_action": c.tasks.needsAction, "selected": c.tasks.selected ?? "", "detail_cards": c.tasks.detail?.cards.count ?? 0,
                    "detail_steps": c.tasks.detail?.steps.count ?? 0, "detail_files": c.tasks.detail?.files ?? [],
                    "island_state": c.model.state.rawValue, "island_toast": c.model.toast?.id ?? "", "live_running": c.model.liveRunning, "live_needs": c.model.liveNeeds,
                    "app_ax_trusted": AXIsProcessTrusted(),
                    "connectors_open": c.connectors.isOpen,
                    "onboarding_open": c.onboarding.isOpen, "edit_paste": NSApp.mainMenu?.items.first?.submenu?.items.contains { $0.keyEquivalent == "v" } ?? false, "onboarding_step": c.onboarding.model.step, "onboarded": OnboardingController.done, "connectors": c.connectors.store.connectors.count,
                    "status_item_visible": c.statusBar.item.isVisible, "status_item_x": c.statusBar.item.button?.window?.frame.minX ?? -1,
                    "overview_tab": c.overview.layout.tab.rawValue, "friend_note": c.overview.friend.note, "friend_busy": c.overview.friend.busy, "history_total": c.overview.history.total, "history_hits": c.overview.history.hits.map { ["id": $0.id, "status": $0.status, "title": $0.title, "created": $0.created] },
                    "monitors": c.overview.monitors.monitors.count]
        case "/focus_textarea":   // tests: put focus into the first text area of an app's window
            guard let ra = NSRunningApplication.runningApplications(withBundleIdentifier: b["bundle"] as? String ?? "").first else { return ["error": "app"] }
            var queue: [AXUIElement] = [AXUIElementCreateApplication(ra.processIdentifier)]
            var seen = 0
            while !queue.isEmpty && seen < 4000 {
                let e = queue.removeFirst(); seen += 1
                var role: AnyObject?; AXUIElementCopyAttributeValue(e, kAXRoleAttribute as CFString, &role)
                if role as? String == "AXTextArea" {
                    let r = AXUIElementSetAttributeValue(e, kAXFocusedAttribute as CFString, kCFBooleanTrue)
                    return ["ok": r == .success]
                }
                var kids: AnyObject?
                if AXUIElementCopyAttributeValue(e, kAXChildrenAttribute as CFString, &kids) == .success, let k = kids as? [AXUIElement] { queue.append(contentsOf: k) }
            }
            return ["error": "no text area"]
        case "/focus_info":   // which element of an app has focus (dictation tests)
            guard let t = DictationTarget.capture(bundle: b["bundle"] as? String), let el = t.element else { return ["error": "no focused element"] }
            func a(_ k: String) -> String { var v: AnyObject?; return AXUIElementCopyAttributeValue(el, k as CFString, &v) == .success ? "\(v!)".prefix(80).description : "" }
            return ["role": a(kAXRoleAttribute), "subrole": a(kAXSubroleAttribute), "desc": a(kAXDescriptionAttribute), "value": a(kAXValueAttribute), "placeholder": a("AXPlaceholderValue")]
        case "/send":   // raw message to the server (e.g. typed turn)
            c.socket.send(json: b)
            return ["ok": true]
        default:
            return ["error": "not found"]
        }
    }
}
