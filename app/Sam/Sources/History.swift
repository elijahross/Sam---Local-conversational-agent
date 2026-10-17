// Long-term memory in the overview: the History tab (every task ever, hybrid search with
// status / date filters and sorting — GET /api/history) and the Monitors tab (saved searches
// Sam re-checks on a schedule, with the listings they collected — /api/monitors).
import AppKit
import SwiftUI

enum OverviewTab: String, CaseIterable, Identifiable {
    case recent = "Recent", history = "History", monitors = "Automations", you = "You"
    var id: String { rawValue }
}

/// Small signed JSON client for the local server (same token as TaskStore).
struct LocalAPI {
    let base: URL

    func get(_ path: String, query: [URLQueryItem] = []) async -> [String: Any]? {
        var c = URLComponents(url: base.appendingPathComponent(path), resolvingAgainstBaseURL: false)!
        if !query.isEmpty { c.queryItems = query }
        var req = URLRequest(url: c.url!, timeoutInterval: 8)
        LocalAuth.sign(&req)
        req.cachePolicy = .reloadIgnoringLocalCacheData
        guard let (d, r) = try? await URLSession.shared.data(for: req), (r as? HTTPURLResponse)?.statusCode == 200 else { return nil }
        return (try? JSONSerialization.jsonObject(with: d)) as? [String: Any]
    }

    @discardableResult
    func send(_ path: String, method: String, body: [String: Any]? = nil) async -> Bool {
        guard let r = await call(path, method: method, body: body) else { return false }
        return r["ok"] as? Bool != false
    }

    /// POST/PUT with the JSON reply (nil on failure). `timeout` for calls that do real work: the
    /// check-in makes several model calls and takes 10–40 s (2026-10-15: the 8 s default cut it
    /// off and "Check in now" always said "Couldn't run it").
    func call(_ path: String, method: String, body: [String: Any]? = nil, timeout: TimeInterval = 8) async -> [String: Any]? {
        var req = URLRequest(url: base.appendingPathComponent(path), timeoutInterval: timeout)
        LocalAuth.sign(&req)
        req.httpMethod = method
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        }
        guard let (d, r) = try? await URLSession.shared.data(for: req) else { return nil }
        let j = ((try? JSONSerialization.jsonObject(with: d)) as? [String: Any]) ?? [:]
        if (r as? HTTPURLResponse)?.statusCode == 200 { return j }
        // An error the server explained ({"ok": false, "message"}): pass it on.
        return j["message"] != nil ? j.merging(["ok": false]) { _, n in n } : nil
    }
}

// MARK: History

struct HistoryHit: Identifiable, Equatable {
    let id: String
    let created: Double
    let status: String
    let kind: String
    let title: String
    let snippet: String
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        created = j["created"] as? Double ?? 0
        status = j["status"] as? String ?? ""
        kind = j["kind"] as? String ?? ""
        title = j["title"] as? String ?? ""
        snippet = j["snippet"] as? String ?? ""
    }
    var date: Date { Date(timeIntervalSince1970: created / 1000) }
}

enum HistoryRange: String, CaseIterable, Identifiable {
    case all = "Any time", day = "Last 24 hours", week = "Last 7 days", month = "Last 30 days", quarter = "Last 3 months", year = "Last year"
    var id: String { rawValue }
    var days: Double? { ["Last 24 hours": 1, "Last 7 days": 7, "Last 30 days": 30, "Last 3 months": 91, "Last year": 365][rawValue] }
}

@MainActor
final class HistoryStore: ObservableObject {
    @Published var query = ""
    @Published var status = ""          // "" = all
    @Published var range: HistoryRange = .all
    @Published var sort = ""            // "" = relevance with a query, newest without
    @Published private(set) var hits: [HistoryHit] = []
    @Published private(set) var total = 0
    @Published private(set) var loading = false
    private let api: LocalAPI
    private var generation = 0

    init(base: URL) { api = LocalAPI(base: base) }

    /// Filters as one value: the view re-runs the search when it changes.
    var key: String { "\(query)|\(status)|\(range.rawValue)|\(sort)" }

    func search(more: Bool = false) async {
        generation += 1
        let gen = generation
        loading = true
        var q: [URLQueryItem] = [URLQueryItem(name: "limit", value: "40"), URLQueryItem(name: "offset", value: String(more ? hits.count : 0))]
        let text = query.trimmingCharacters(in: .whitespaces)
        if !text.isEmpty { q.append(URLQueryItem(name: "q", value: text)) }
        if !status.isEmpty { q.append(URLQueryItem(name: "status", value: status)) }
        if !sort.isEmpty { q.append(URLQueryItem(name: "sort", value: sort)) }
        if let d = range.days { q.append(URLQueryItem(name: "from", value: String(Int64((Date().timeIntervalSince1970 - d * 86400) * 1000)))) }
        let j = await api.get("api/history", query: q)
        guard gen == generation else { return }   // a newer search started meanwhile
        let page = ((j?["hits"] as? [[String: Any]]) ?? []).compactMap(HistoryHit.init)
        hits = more ? hits + page : page
        total = j?["total"] as? Int ?? hits.count
        loading = false
    }

    func remove(_ id: String) {
        hits.removeAll { $0.id == id }
        total = max(0, total - 1)
    }
}

struct HistoryList: View {
    @ObservedObject var history: HistoryStore
    @ObservedObject var store: TaskStore

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(Theme.text3)
                TextField("Search everything Sam did", text: $history.query).textFieldStyle(.plain).font(Theme.body)
                    .accessibilityLabel("Search history")
                if !history.query.isEmpty {
                    Button { history.query = "" } label: { Image(systemName: "xmark.circle.fill") }.buttonStyle(.plain).foregroundStyle(Theme.text3)
                        .accessibilityLabel("Clear search")
                }
            }
            .padding(8)
            .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
            HStack(spacing: 6) {
                Menu {
                    Picker("Status", selection: $history.status) {
                        Text("All").tag(""); Text("Done").tag("done"); Text("Failed").tag("failed"); Text("Needs your OK").tag("needs_action")
                    }
                } label: { filterLabel("line.3.horizontal.decrease", history.status.isEmpty ? "Status" : TaskText.status(history.status)) }
                Menu {
                    Picker("Date", selection: $history.range) { ForEach(HistoryRange.allCases) { Text($0.rawValue).tag($0) } }
                } label: { filterLabel("calendar", history.range == .all ? "Date" : history.range.rawValue) }
                Menu {
                    Picker("Sort", selection: $history.sort) { Text("Best match").tag(""); Text("Newest first").tag("newest"); Text("Oldest first").tag("oldest") }
                } label: { filterLabel("arrow.up.arrow.down", history.sort == "oldest" ? "Oldest" : history.sort == "newest" ? "Newest" : "Sort") }
            }
            .menuStyle(.borderlessButton).fixedSize()
            Text(history.loading && history.hits.isEmpty ? "Searching…" : "\(history.total) task\(history.total == 1 ? "" : "s")")
                .font(Theme.caption).foregroundStyle(Theme.text3).padding(.leading, 4)
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(history.hits) { h in
                        Button { store.selected = h.id } label: {
                            HStack(alignment: .top, spacing: 9) {
                                StatusGlyph(status: h.status).padding(.top, 1)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(h.title.isEmpty ? "Background task" : h.title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                                    Text(h.date.formatted(date: .abbreviated, time: .shortened) + (h.kind == "monitor" ? " · monitor" : ""))
                                        .font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                                    if !h.snippet.isEmpty { Text(h.snippet).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(2) }
                                }
                                Spacer(minLength: 0)
                            }
                            .padding(8)
                            .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill((store.selected == h.id ? Theme.fillSelected : Color.clear)))
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                    if history.hits.count < history.total {
                        Button("Load more") { Task { await history.search(more: true) } }.buttonStyle(.plain).font(Theme.caption).foregroundStyle(Theme.accent).padding(8)
                    }
                    if history.hits.isEmpty && !history.loading {
                        Text(history.query.isEmpty ? "Nothing in this range." : "No task matches “\(history.query)”.").font(Theme.body).foregroundStyle(Theme.text2).padding(8)
                    }
                }
            }
            .scrollIndicators(.never)
        }
        // Debounced: typing waits 300 ms; filter changes search at once.
        .task(id: history.key) {
            if !history.query.isEmpty { try? await Task.sleep(nanoseconds: 300_000_000) }
            if !Task.isCancelled { await history.search() }
        }
    }

    private func filterLabel(_ icon: String, _ text: String) -> some View {
        HStack(spacing: 4) { Image(systemName: icon); Text(text) }
            .font(Theme.caption.weight(.semibold)).foregroundStyle(Theme.text2)
            .padding(.horizontal, 8).padding(.vertical, 4)
            .background(Capsule().fill(Theme.fill))
    }
}

// MARK: Monitors

struct MonitorItem: Identifiable, Equatable {
    let id: String
    let name: String
    let request: String
    let url: String
    let site: String
    let interval: Int
    let active: Bool
    let lastRun: Double?
    let nextRun: Double
    let lastStatus: String
    let lastError: String?
    let listings: Int
    let new24h: Int
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        let s = { (k: String) in j[k] as? String ?? "" }
        name = s("name"); request = s("request"); url = s("url"); site = s("site"); lastStatus = s("last_status")
        interval = j["interval_min"] as? Int ?? 60
        active = j["active"] as? Bool ?? true
        lastRun = j["last_run"] as? Double
        nextRun = j["next_run"] as? Double ?? 0
        lastError = j["last_error"] as? String
        listings = j["listings"] as? Int ?? 0
        new24h = j["new_24h"] as? Int ?? 0
    }
}

struct MonitorListing: Identifiable {
    let id: String
    let url: String
    let title: String
    let price: String
    let location: String
    let posted: String
    let firstSeen: Double
    let baseline: Bool
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        let s = { (k: String) in j[k] as? String ?? "" }
        url = s("url"); title = s("title"); price = s("price"); location = s("location"); posted = s("posted")
        firstSeen = j["first_seen"] as? Double ?? 0
        baseline = j["baseline"] as? Bool ?? false
    }
    var isNew: Bool { !baseline && Date().timeIntervalSince1970 * 1000 - firstSeen < 86_400_000 }
}

struct AutomationItem: Identifiable, Equatable {
    let id: String
    let name: String
    let request: String
    let kind: String
    let when: String
    let active: Bool
    let runs: Int
    let lastError: String?
    let nextCheck: Double
    let history: [String]
    init?(_ j: [String: Any]) {
        guard let a = j["automation"] as? [String: Any], let id = a["id"] as? String else { return nil }
        self.id = id
        name = a["name"] as? String ?? ""; request = a["request"] as? String ?? ""; kind = a["kind"] as? String ?? ""
        if let s = a["schedule"] as? [String: Any] {
            if let m = s["every_min"] as? Int { when = "every \(m) min" } else {
                let days = (s["days"] as? String) ?? ((s["days"] as? [String])?.joined(separator: ", ") ?? "daily")
                when = "\(days) at \(s["at"] as? String ?? "")"
            }
        } else { when = a["condition"] as? String ?? "" }
        active = a["active"] as? Bool ?? true; runs = a["runs"] as? Int ?? 0; lastError = a["last_error"] as? String
        nextCheck = a["next_check"] as? Double ?? 0
        history = ((j["history"] as? [[String: Any]]) ?? []).map { h in
            let at = Date(timeIntervalSince1970: (h["at"] as? Double ?? 0) / 1000).formatted(date: .abbreviated, time: .shortened)
            return "\(at) — \((h["detail"] as? String ?? "").prefix(140))"
        }
    }
}

struct CampaignItem: Identifiable, Equatable {
    let id: String
    let product: String
    let state: String
    let leads: Int
    let good: Int
    let target: Int?
    let note: String
    let csv: String?
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        self.id = id
        product = j["product"] as? String ?? ""; state = j["state"] as? String ?? "quick list"; leads = j["leads"] as? Int ?? 0; good = j["good"] as? Int ?? 0
        target = j["target"] as? Int; note = j["note"] as? String ?? ""; csv = j["csv"] as? String
    }
}

@MainActor
final class MonitorStore: ObservableObject {
    @Published private(set) var monitors: [MonitorItem] = []
    @Published private(set) var automations: [AutomationItem] = []
    @Published private(set) var campaigns: [CampaignItem] = []
    @Published var selected: String? { didSet { if selected != oldValue { listings = []; runs = []; Task { await loadDetail() } } } }
    @Published private(set) var listings: [MonitorListing] = []
    @Published private(set) var runs: [[String: Any]] = []
    @Published private(set) var loaded = false
    private let api: LocalAPI

    init(base: URL) { api = LocalAPI(base: base) }

    var current: MonitorItem? { monitors.first { $0.id == selected } }

    func refresh() async {
        let j = await api.get("api/monitors")
        let list = ((j?["monitors"] as? [[String: Any]]) ?? []).compactMap(MonitorItem.init)
        if list != monitors { monitors = list }
        let a = ((await api.get("api/automations"))?["automations"] as? [[String: Any]] ?? []).compactMap(AutomationItem.init)
        if a != automations { automations = a }
        let c = ((await api.get("api/campaigns"))?["campaigns"] as? [[String: Any]] ?? []).compactMap(CampaignItem.init).filter { $0.target != nil }
        if c != campaigns { campaigns = c }
        loaded = true
        let ids = automations.map(\.id) + monitors.map(\.id) + campaigns.map(\.id)
        if selected == nil || !ids.contains(selected ?? "") { selected = ids.first }
    }

    var automation: AutomationItem? { automations.first { $0.id == selected } }
    var campaign: CampaignItem? { campaigns.first { $0.id == selected } }

    func actAutomation(_ id: String, _ action: String) {
        Task { if action == "delete" { await api.send("api/automations/\(id)", method: "DELETE") } else { await api.send("api/automations/\(id)", method: "POST", body: ["action": action]) }; await refresh() }
    }

    func actCampaign(_ id: String, _ action: String) {
        Task { await api.send("api/campaigns/\(id)", method: "POST", body: ["action": action]); await refresh() }
    }

    func loadDetail() async {
        guard let id = selected, id.hasPrefix("m"), let j = await api.get("api/monitors/\(id)") else { return }
        listings = ((j["listings"] as? [[String: Any]]) ?? []).compactMap(MonitorListing.init)
        runs = (j["runs"] as? [[String: Any]]) ?? []
    }

    func act(_ id: String, _ action: String, minutes: Int? = nil) {
        Task {
            var body: [String: Any] = ["action": action]
            if let minutes { body["minutes"] = minutes }
            await api.send("api/monitors/\(id)", method: "POST", body: body)
            await refresh()
            await loadDetail()
        }
    }

    func delete(_ id: String) {
        monitors.removeAll { $0.id == id }
        if selected == id { selected = monitors.first?.id }
        Task { await api.send("api/monitors/\(id)", method: "DELETE"); await refresh() }
    }
}

struct MonitorList: View {
    @ObservedObject var monitors: MonitorStore

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                if !monitors.automations.isEmpty { header("Schedules & watches") }
                ForEach(monitors.automations) { a in
                    row(id: a.id, icon: a.kind == "schedule" ? "clock" : "eye", tint: a.active ? (a.lastError == nil ? Theme.accent : Theme.warn) : Theme.text3, title: a.name, sub: "\(a.when)\(a.active ? "" : " · paused")", badge: a.runs > 0 ? "\(a.runs)×" : nil)
                }
                if !monitors.campaigns.isEmpty { header("Lead lists") }
                ForEach(monitors.campaigns) { c in
                    row(id: c.id, icon: "person.2", tint: c.state == "active" ? Theme.accent : Theme.text3, title: c.product, sub: "\(c.leads) of \(c.target ?? 0) · \(c.state)", badge: c.good > 0 ? "\(c.good) fit" : nil)
                }
                if !monitors.monitors.isEmpty && !(monitors.automations.isEmpty && monitors.campaigns.isEmpty) { header("Saved-search monitors") }
                ForEach(monitors.monitors) { m in
                    Button { monitors.selected = m.id } label: {
                        HStack(alignment: .top, spacing: 9) {
                            Image(systemName: m.active ? (m.lastStatus == "failed" ? "exclamationmark.triangle.fill" : "dot.radiowaves.left.and.right") : "pause.circle")
                                .foregroundStyle(m.active ? (m.lastStatus == "failed" ? Theme.warn : Theme.accent) : Theme.text3).frame(width: 16).padding(.top, 1)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(m.name).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                                Text("\(m.site) · \(MonitorText.every(m.interval))\(m.active ? "" : " · paused")").font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                            }
                            Spacer(minLength: 0)
                            if m.new24h > 0 { Pill(text: "\(m.new24h) new", color: Theme.good) }
                        }
                        .padding(8)
                        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill((monitors.selected == m.id ? Theme.fillSelected : Color.clear)))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                if monitors.loaded && monitors.monitors.isEmpty && monitors.automations.isEmpty && monitors.campaigns.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Nothing automatic yet.").font(Theme.body).foregroundStyle(Theme.text2)
                        Text("Ask Sam: “Every weekday at 8, brief me on my day”, “Tell me when a new flat in Vienna under 1,200 € shows up on willhaben”, or “Build a list of 5,000 leads for …”.").font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                    .padding(8)
                }
            }
        }
        .scrollIndicators(.never)
        .task { while !Task.isCancelled { await monitors.refresh(); try? await Task.sleep(nanoseconds: 8_000_000_000) } }
    }

    private func header(_ t: String) -> some View {
        Text(t.uppercased()).font(Theme.overline).foregroundStyle(Theme.text3).padding(.leading, 8).padding(.top, 8)
    }

    private func row(id: String, icon: String, tint: Color, title: String, sub: String, badge: String?) -> some View {
        Button { monitors.selected = id } label: {
            HStack(alignment: .top, spacing: 9) {
                Image(systemName: icon).foregroundStyle(tint).frame(width: 16).padding(.top, 1)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                    Text(sub).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer(minLength: 0)
                if let badge { Pill(text: badge, color: Theme.accent) }
            }
            .padding(8)
            .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill((monitors.selected == id ? Theme.fillSelected : Color.clear)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// An automation (schedule / watch) or a long-running lead list in the detail pane.
struct AutomationDetail: View {
    @ObservedObject var monitors: MonitorStore
    @State private var confirmDelete = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                if let a = monitors.automation {
                    HStack(spacing: 8) {
                        Pill(text: a.active ? (a.lastError == nil ? "Active" : "Check failed") : "Paused", color: a.active ? (a.lastError == nil ? Theme.good : Theme.warn) : Theme.text3)
                        Chip(icon: a.kind == "schedule" ? "clock" : "eye", text: a.kind == "schedule" ? "Schedule" : "Watch")
                        Spacer()
                        Button { monitors.actAutomation(a.id, a.active ? "pause" : "resume") } label: { Label(a.active ? "Pause" : "Resume", systemImage: a.active ? "pause" : "play") }.samButton().controlSize(.small)
                        Button { confirmDelete = true } label: { Label("Delete", systemImage: "trash") }.samButton().controlSize(.small)
                            .confirmationDialog("Delete this automation?", isPresented: $confirmDelete) {
                                Button("Delete", role: .destructive) { monitors.actAutomation(a.id, "delete") }
                                Button("Keep", role: .cancel) {}
                            }
                    }
                    Text(a.name).font(Theme.h2).foregroundStyle(Theme.text)
                    labeled("When", a.when)
                    labeled("Does", a.request)
                    if a.active && a.nextCheck > 0 { labeled("Next check", Date(timeIntervalSince1970: a.nextCheck / 1000).formatted(date: .abbreviated, time: .shortened)) }
                    if let e = a.lastError { Text(e).font(Theme.caption).foregroundStyle(Theme.warn) }
                    if !a.history.isEmpty {
                        VStack(alignment: .leading, spacing: 6) {
                            Text("Ran").font(Theme.label).foregroundStyle(Theme.text3)
                            ForEach(a.history, id: \.self) { Text($0).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(2) }
                        }
                    }
                } else if let c = monitors.campaign {
                    HStack(spacing: 8) {
                        Pill(text: c.state.capitalized, color: c.state == "active" ? Theme.good : Theme.text3)
                        Chip(icon: "person.2", text: "LinkedIn lead list")
                        Spacer()
                        if c.state == "active" || c.state == "paused" {
                            Button { monitors.actCampaign(c.id, c.state == "active" ? "pause" : "resume") } label: { Label(c.state == "active" ? "Pause" : "Resume", systemImage: c.state == "active" ? "pause" : "play") }.samButton().controlSize(.small)
                        }
                        if let csv = c.csv {
                            Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: csv)]) } label: { Label("CSV", systemImage: "tablecells") }.samButton().controlSize(.small)
                        }
                    }
                    Text(c.product).font(Theme.h2).foregroundStyle(Theme.text)
                    ProgressView(value: Double(c.leads), total: Double(max(c.target ?? 1, 1))).tint(Theme.accent)
                    labeled("People", "\(c.leads) of \(c.target ?? 0) · \(c.good) good fits (score ≥ 70)")
                    if !c.note.isEmpty { labeled("Note", c.note) }
                }
            }
            .padding(28).frame(maxWidth: .infinity, alignment: .leading)
        }
        .scrollIndicators(.never)
        .frame(maxHeight: .infinity, alignment: .top)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
    }

    private func labeled(_ k: String, _ v: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(k).font(Theme.label).foregroundStyle(Theme.text3)
            Text(v).font(Theme.body).foregroundStyle(Theme.text).textSelection(.enabled)
        }
    }
}

enum MonitorText {
    static let intervals = [15, 30, 60, 180, 360, 720, 1440]
    static func every(_ m: Int) -> String {
        m < 60 ? "every \(m) min" : m == 60 ? "hourly" : m < 1440 ? "every \(m / 60) h" : m == 1440 ? "daily" : "every \(m / 1440) days"
    }
    static func when(_ ms: Double?) -> String {
        guard let ms, ms > 0 else { return "—" }
        return TaskText.ago(Date(timeIntervalSince1970: ms / 1000))
    }
}

struct MonitorDetail: View {
    @ObservedObject var monitors: MonitorStore
    @State private var confirmDelete = false
    @State private var showAll = false

    var body: some View {
        Group {
            if let m = monitors.current {
                ScrollView {
                    VStack(alignment: .leading, spacing: 22) {
                        header(m)
                        stats(m)
                        let fresh = monitors.listings.filter { !$0.baseline }
                        if !fresh.isEmpty {
                            section("New listings") { ForEach(fresh.prefix(60)) { row($0) } }
                        }
                        let rest = monitors.listings.filter { $0.baseline }
                        if !rest.isEmpty {
                            DisclosureGroup(isExpanded: $showAll) {
                                VStack(alignment: .leading, spacing: 6) { ForEach(rest.prefix(200)) { row($0) } }.padding(.top, 8)
                            } label: {
                                Text("Older listings — there at the start or re-listed (\(rest.count))").font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text2)
                            }
                        }
                        if !monitors.runs.isEmpty { section("Recent checks") { runsView } }
                    }
                    .padding(28)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .scrollIndicators(.never)
            } else {
                VStack(spacing: 10) {
                    Image(systemName: "dot.radiowaves.left.and.right").font(.system(size: 30)).foregroundStyle(Theme.text3)
                    Text(monitors.monitors.isEmpty ? "No monitors yet" : "Pick a monitor").font(Theme.title).foregroundStyle(Theme.text2)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
    }

    private func header(_ m: MonitorItem) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Pill(text: m.active ? (m.lastStatus == "failed" ? "Check failed" : "Watching") : "Paused", color: m.active ? (m.lastStatus == "failed" ? Theme.warn : Theme.good) : Theme.text3)
                Chip(icon: "globe", text: m.site)
                Spacer()
                Menu {
                    ForEach(MonitorText.intervals, id: \.self) { i in Button(MonitorText.every(i)) { monitors.act(m.id, "interval", minutes: i) } }
                } label: { Label(MonitorText.every(m.interval), systemImage: "clock") }
                    .menuStyle(.borderlessButton).fixedSize().accessibilityLabel("Check interval")
                Button { monitors.act(m.id, "run") } label: { Label("Check now", systemImage: "arrow.clockwise") }.samButton().controlSize(.small)
                Button { monitors.act(m.id, m.active ? "pause" : "resume") } label: { Label(m.active ? "Pause" : "Resume", systemImage: m.active ? "pause" : "play") }
                    .samButton().controlSize(.small)
                Button { confirmDelete = true } label: { Label("Delete", systemImage: "trash") }.samButton().controlSize(.small)
                    .confirmationDialog("Delete this monitor?", isPresented: $confirmDelete) {
                        Button("Delete monitor and its \(m.listings) listings", role: .destructive) { monitors.delete(m.id) }
                        Button("Keep", role: .cancel) {}
                    }
            }
            Text(m.name).font(Theme.h2).foregroundStyle(Theme.text).textSelection(.enabled)
            if !m.request.isEmpty && m.request != m.name { Text(m.request).font(Theme.body).foregroundStyle(Theme.text2).lineLimit(2) }
            Button { if let u = URL(string: m.url) { NSWorkspace.shared.open(u) } } label: {
                Text(m.url).font(Theme.caption).foregroundStyle(Theme.accent).lineLimit(1).truncationMode(.middle)
            }.buttonStyle(.plain).help("Open the saved search in your browser")
            if m.lastStatus == "failed", let e = m.lastError { Text(e).font(Theme.caption).foregroundStyle(Theme.warn).lineLimit(3) }
        }
    }

    private func stats(_ m: MonitorItem) -> some View {
        HStack(spacing: 10) {
            stat("New (24 h)", "\(m.new24h)")
            stat("Listings stored", "\(m.listings)")
            stat("Last check", MonitorText.when(m.lastRun))
            stat("Next check", m.active ? (m.nextRun <= Date().timeIntervalSince1970 * 1000 ? "now" : Date(timeIntervalSince1970: m.nextRun / 1000).formatted(date: .omitted, time: .shortened)) : "paused")
        }
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(value).font(.system(size: 18, weight: .semibold, design: .rounded)).foregroundStyle(Theme.text).lineLimit(1)
            Text(label).font(Theme.caption).foregroundStyle(Theme.text3)
        }
        .padding(12).frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
    }

    private func row(_ l: MonitorListing) -> some View {
        Button { if let u = URL(string: l.url) { NSWorkspace.shared.open(u) } } label: {
            HStack(alignment: .top, spacing: 10) {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        if l.isNew { Pill(text: "new", color: Theme.good) }
                        Text(l.title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                    }
                    Text([l.location, l.posted, "seen " + MonitorText.when(l.firstSeen)].filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer(minLength: 8)
                Text(l.price).font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
                Image(systemName: "arrow.up.right.square").foregroundStyle(Theme.text3)
            }
            .padding(10)
            .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(l.url)
    }

    private var runsView: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(Array(monitors.runs.prefix(10).enumerated()), id: \.offset) { _, r in
                let err = r["error"] as? String
                HStack(spacing: 8) {
                    StatusGlyph(status: err == nil ? "done" : "failed", size: 12)
                    Text(MonitorText.when(r["at"] as? Double)).font(Theme.caption).foregroundStyle(Theme.text2).frame(width: 90, alignment: .leading)
                    Text(err ?? "\(r["found"] as? Int ?? 0) found · \(r["new"] as? Int ?? 0) new\((r["changed"] as? Int ?? 0) > 0 ? " · \(r["changed"] as? Int ?? 0) price changes" : "")")
                        .font(Theme.caption).foregroundStyle(err == nil ? Theme.text2 : Theme.warn).lineLimit(1)
                }
            }
        }
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(Theme.label).foregroundStyle(Theme.text3)
            content()
        }
    }
}
