// Island state machine (design-system.md §3) and everything the views show.
import SwiftUI

enum IslandState: String { case hidden, peek, listening, thinking, speaking, result, toast, dictating }

struct CardBlock: Identifiable { let id = UUID(); let raw: [String: Any] }
struct Card: Identifiable {
    let id: String
    let kind: String          // result | confirm | error
    let blocks: [CardBlock]
    let open: String?
    let confirm: [String: Any]?
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        kind = j["kind"] as? String ?? "result"
        blocks = ((j["blocks"] as? [[String: Any]]) ?? []).prefix(3).map { CardBlock(raw: $0) }
        // A task result opens its task in the overview ("task:<id>"); otherwise a file.
        open = ((j["open"] as? [String: Any])?["task"] as? String).map { "task:\($0)" } ?? (j["open"] as? [String: Any])?["file"] as? String
        confirm = j["confirm"] as? [String: Any]
    }
}

@MainActor
final class IslandModel: ObservableObject {
    @Published var state: IslandState = .hidden
    @Published var handsFree = false
    /// Hands-free after a reply: listening briefly for a follow-up before the mic turns off.
    @Published var followUp = false
    @Published var level: Float = 0           // mic level (listening) or TTS level (speaking), 0…1
    @Published var bands: [Float] = Array(repeating: 0, count: 24)
    @Published var transcript = ""
    @Published var answer = ""
    @Published var cards: [Card] = []
    @Published var contextChip: String?
    @Published var taskChip: String?
    @Published var connected = false
    @Published var error: String?
    @Published var pttHeld = false
    @Published var panelHeight: CGFloat = 96
    /// Background tasks, shown as a live activity around the notch while the island is hidden.
    @Published var liveRunning = 0
    @Published var liveNeeds = 0
    /// The task a drop-down notification (state .toast) is about; hovering keeps it open.
    @Published var toast: TaskItem?
    @Published var toastHover = false
    /// Dictation: the app text goes into, and the last inserted text (for the "Inserted" line).
    @Published var dictationTarget: String?
    @Published var dictationResult: String?
    var live: Bool { liveRunning + liveNeeds > 0 }
    /// Voice menu: "sam-male" or "sam-female". Saved, and sent on every connect (an older
    /// saved "rowan" counts as Sam's male voice).
    /// Voice off (meetings): Sam neither listens nor speaks; results drop down as notifications.
    @Published var voiceOff: Bool = UserDefaults.standard.bool(forKey: "voiceOff") {
        didSet { UserDefaults.standard.set(voiceOff, forKey: "voiceOff") }
    }
    @Published var voiceChoice: String = { let v = UserDefaults.standard.string(forKey: "character") ?? "sam-male"; return v.hasPrefix("sam-") ? v : "sam-male" }() {
        didSet { UserDefaults.standard.set(voiceChoice, forKey: "character") }
    }
    var characterName: String { "Sam" }
    var characterMessage: [String: Any] {
        ["type": "character", "character": "sam", "variant": voiceChoice == "sam-female" ? "female" : "male"]
    }

    /// Timeline of state changes (ms since app start) — the E2E tests read it.
    private(set) var log: [(String, Double)] = []
    let t0 = Date()
    func set(_ s: IslandState) {
        guard s != state else { return }
        state = s
        log.append((s.rawValue, Date().timeIntervalSince(t0) * 1000))
        if log.count > 200 { log.removeFirst(50) }
        NSAccessibility.post(element: NSApp as Any, notification: .announcementRequested,
                             userInfo: [.announcement: "Sam \(s.rawValue)", .priority: NSAccessibilityPriorityLevel.low.rawValue])
    }
    func mark(_ event: String) { log.append((event, Date().timeIntervalSince(t0) * 1000)) }
    var expanded: Bool { state != .hidden && state != .peek && state != .toast && state != .dictating }
    var hasConfirm: Bool { cards.contains { $0.kind == "confirm" } }
}
