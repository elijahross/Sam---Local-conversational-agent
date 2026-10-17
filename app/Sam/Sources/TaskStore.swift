// Background tasks as the server's task registry reports them (GET /api/tasks): what the menu
// bar shows, what triggers drop-down notifications, and what the overview renders.
import AppKit
import SwiftUI

struct TaskStep: Identifiable {
    let id: Int
    let tool: String
    let status: String
    let summary: String
    let file: String?
    let at: Double
}

struct TaskAction: Identifiable {
    let id: String
    let tool: String
    let summary: String
    let status: String
}

struct TaskItem: Identifiable, Equatable {
    let id: String
    let kind: String
    let request: String
    /// Generated headline (the report's title when there is one); empty on old records.
    let headline: String
    let context: String
    let status: String          // running | needs_action | done | failed
    let current: String
    let result: String
    let plan: [String]
    let steps: [TaskStep]
    let actions: [TaskAction]
    let files: [String]
    let notes: [String]
    let tokens: Int
    let created: Double
    let updated: Double
    var cards: [Card] = []
    /// The presented result (detail only); "building" / "ready" in the list.
    var report: [String: Any]?
    var reportStatus: String = ""
    /// A typed result (music, weather, reminder, event) the overview draws large.
    var view: [String: Any]?

    static func == (a: TaskItem, b: TaskItem) -> Bool { a.id == b.id && a.updated == b.updated && a.cards.map(\.id) == b.cards.map(\.id) }

    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        let s = { (k: String) in j[k] as? String ?? "" }
        kind = s("kind"); request = s("request"); headline = s("title"); context = s("context"); status = s("status"); current = s("current"); result = s("result")
        plan = j["plan"] as? [String] ?? []
        steps = ((j["steps"] as? [[String: Any]]) ?? []).enumerated().map { i, x in
            TaskStep(id: i, tool: x["tool"] as? String ?? "", status: x["status"] as? String ?? "", summary: x["summary"] as? String ?? "",
                     file: (x["file"] as? String) ?? (x["dir"] as? String), at: x["at"] as? Double ?? 0)
        }
        actions = ((j["actions"] as? [[String: Any]]) ?? []).map {
            TaskAction(id: $0["id"] as? String ?? "", tool: $0["tool"] as? String ?? "", summary: $0["summary"] as? String ?? "", status: $0["status"] as? String ?? "")
        }
        files = j["files"] as? [String] ?? []
        notes = j["notes"] as? [String] ?? []
        tokens = j["tokens"] as? Int ?? 0
        created = j["created"] as? Double ?? 0
        updated = j["updated"] as? Double ?? 0
        cards = ((j["cards"] as? [String: Any]) ?? [:]).values.compactMap { ($0 as? [String: Any]).flatMap(Card.init) }
        report = j["report"] as? [String: Any]
        reportStatus = j["report_status"] as? String ?? ""
        view = j["view"] as? [String: Any]
    }

    var title: String {
        if !headline.isEmpty { return headline }
        let t = requestLine
        return t.isEmpty ? "Background task" : t.prefix(1).uppercased() + t.dropFirst()
    }
    /// The user's own words (first line), shown quoted beneath the headline.
    var requestLine: String { request.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: "\n").first ?? request }
    var pending: [TaskAction] { actions.filter { $0.status == "pending" } }
    var createdDate: Date { Date(timeIntervalSince1970: created / 1000) }
}

@MainActor
final class TaskStore: ObservableObject {
    @Published private(set) var tasks: [TaskItem] = []
    @Published var selected: String? { didSet { if selected != oldValue { detail = nil; Task { await refresh() } } } }
    @Published private(set) var detail: TaskItem?
    @Published private(set) var reachable = false
    /// A task reached done / failed / needs_action (not on first load).
    var onChange: ((TaskItem) -> Void)?
    /// Poll fast while the overview is open.
    var fastPoll = false
    let base: URL
    /// A task was deleted (History drops it from its results too).
    var onDeleted: ((String) -> Void)?
    private var seen: [String: String] = [:]
    private var primed = false

    init(base: URL) { self.base = base }

    /// Running plus queued (waiting for a slot or memory): both are "work in progress".
    var running: Int { tasks.filter { $0.status == "running" || $0.status == "queued" }.count }
    var queued: Int { tasks.filter { $0.status == "queued" }.count }
    var needsAction: Int { tasks.filter { $0.status == "needs_action" }.count }

    func start() {
        Task { [weak self] in
            while let self {
                await self.refresh()
                let fast = self.fastPoll || self.running > 0 || self.needsAction > 0
                try? await Task.sleep(nanoseconds: fast ? 1_500_000_000 : 6_000_000_000)
            }
        }
    }

    private func get(_ path: String) async -> [String: Any]? {
        var req = URLRequest(url: base.appendingPathComponent(path), timeoutInterval: 3)
        LocalAuth.sign(&req)
        req.cachePolicy = .reloadIgnoringLocalCacheData
        guard let (d, r) = try? await URLSession.shared.data(for: req), (r as? HTTPURLResponse)?.statusCode == 200 else { return nil }
        return (try? JSONSerialization.jsonObject(with: d)) as? [String: Any]
    }

    func refresh() async {
        guard let j = await get("api/tasks"), let arr = j["tasks"] as? [[String: Any]] else { reachable = false; return }
        reachable = true
        let list = arr.compactMap(TaskItem.init)
        for t in list {
            if primed, seen[t.id] != t.status, ["done", "failed", "needs_action"].contains(t.status) { onChange?(t) }
            seen[t.id] = t.status
        }
        primed = true
        if list != tasks { tasks = list }
        // Recent or History: the server answers from either.
        if let s = selected, let d = await get("api/tasks/\(s)"), let t = TaskItem(d), t != detail { detail = t }
    }

    /// Running and queued tasks can't be deleted (the server refuses too).
    static func deletable(_ t: TaskItem) -> Bool { !["running", "queued"].contains(t.status) }

    /// Deletes a task record (a waiting task's pending actions are withdrawn server-side).
    func delete(_ id: String) {
        tasks.removeAll { $0.id == id }
        seen[id] = nil
        if selected == id { selected = nil }
        onDeleted?(id)
        send("api/tasks/\(id)", method: "DELETE")
    }

    /// Clears finished and failed tasks off the recent list (they stay in History).
    func clearFinished() {
        let gone = Set(tasks.filter { $0.status == "done" || $0.status == "failed" }.map(\.id))
        tasks.removeAll { gone.contains($0.id) }
        if let s = selected, gone.contains(s) { selected = nil }
        send("api/tasks/clear", method: "POST")
    }

    private func send(_ path: String, method: String) {
        var req = URLRequest(url: base.appendingPathComponent(path), timeoutInterval: 5)
        LocalAuth.sign(&req)
        req.httpMethod = method
        Task {
            _ = try? await URLSession.shared.data(for: req)
            await refresh()
        }
    }

    func confirm(task: String, action: String, decision: String, args: [String: Any] = [:]) {
        var req = URLRequest(url: base.appendingPathComponent("api/tasks/\(task)/confirm"))
        LocalAuth.sign(&req)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("1", forHTTPHeaderField: "X-Sam")
        var body: [String: Any] = ["action_id": action, "decision": decision]
        if !args.isEmpty { body["args"] = args }
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        Task {
            _ = try? await URLSession.shared.data(for: req)
            try? await Task.sleep(nanoseconds: 400_000_000)
            await refresh()
        }
    }
}

// MARK: shared bits of task UI

/// Trash button for a task. A task that still waits for the user asks first (deleting it also
/// withdraws its pending actions); finished and failed ones go at once.
struct DeleteTaskButton: View {
    let task: TaskItem
    @ObservedObject var store: TaskStore
    var label = false
    @State private var asking = false

    var body: some View {
        Button {
            if task.status == "needs_action" { asking = true } else { store.delete(task.id) }
        } label: {
            if label { Label("Delete", systemImage: "trash") } else { Image(systemName: "trash").font(Theme.caption.weight(.semibold)) }
        }
        .help("Delete this task")
        .accessibilityLabel("Delete task \(task.title)")
        .confirmationDialog("Delete this task?", isPresented: $asking) {
            Button("Delete and withdraw its action", role: .destructive) { store.delete(task.id) }
            Button("Keep", role: .cancel) {}
        } message: {
            Text("It's waiting for your OK. Deleting it cancels what it drafted.")
        }
    }
}

struct StatusGlyph: View {
    let status: String
    var size: CGFloat = 14
    var body: some View {
        switch status {
        case "running": ProgressView().controlSize(.small).frame(width: size, height: size)
        case "queued": Image(systemName: "clock").foregroundStyle(Theme.text2).font(.system(size: size))
        case "done": Image(systemName: "checkmark.circle.fill").foregroundStyle(Theme.good).font(.system(size: size))
        case "failed", "unknown": Image(systemName: "xmark.octagon.fill").foregroundStyle(Theme.bad).font(.system(size: size))
        case "needs_action", "input_required", "pending": Image(systemName: "hand.raised.circle.fill").foregroundStyle(.orange).font(.system(size: size))
        case "cancelled": Image(systemName: "minus.circle.fill").foregroundStyle(Theme.text3).font(.system(size: size))
        default: Image(systemName: "circle").foregroundStyle(Theme.text3).font(.system(size: size))
        }
    }
}

enum TaskText {
    static func tool(_ t: String) -> String {
        [
            "chat_send": "Message", "chat_read": "Read chat", "email_send": "Email", "email_read": "Read email", "email_search": "Search mail",
            "calendar_create": "Calendar event", "calendar_events": "Calendar", "reminder_create": "Reminder", "research": "Web research",
            "scrape": "Collect from the web", "browse": "Browser", "web_task": "Website task", "web_fetch": "Read page", "details": "Item details",
            "doc_review": "Document review", "papers_similar": "Related papers", "note_save": "Note", "code": "Program", "find_files": "Find files",
            "weather": "Weather", "recall_memory": "Memory", "contacts_lookup": "Contacts",
        ][t] ?? t.replacingOccurrences(of: "_", with: " ").capitalized
    }
    static func status(_ s: String) -> String {
        ["queued": "Queued", "running": "Running", "needs_action": "Needs your OK", "done": "Done", "failed": "Failed"][s] ?? s.capitalized
    }
    static func ago(_ d: Date) -> String {
        let f = RelativeDateTimeFormatter(); f.unitsStyle = .short
        return Date().timeIntervalSince(d) < 30 ? "now" : f.localizedString(for: d, relativeTo: Date())
    }
}
