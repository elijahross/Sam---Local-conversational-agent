// The friend layer (server: /api/friend): who the user is in their own words (name, birthday,
// tastes, a few psyche answers), how far Sam goes as a friend (tease, spark, nudges, the daily
// check-in), and everything Sam knows about them, line by line, each deletable.
import SwiftUI

struct ProfileLine: Identifiable, Equatable { let text: String; let source: String; var id: String { text } }
/// Something pending in their life that Sam will ask about once, when its day comes
/// (server: memory.rs open threads). `offered` = Sam already had it this session.
struct OpenThread: Identifiable, Equatable { let id: Int; let text: String; let due: Date?; let offered: Bool }

@MainActor
final class FriendStore: ObservableObject {
    let api: LocalAPI
    private var loading = false
    @Published var name = "" { didSet { if !loading { changed() } } }
    @Published var home = "" { didSet { if !loading { changed() } } }
    @Published var hasBirthday = false { didSet { if !loading { changed() } } }
    @Published var birthday = Calendar.current.date(from: DateComponents(year: 1990, month: 6, day: 15)) ?? Date() { didSet { if !loading { changed() } } }
    @Published var tastes: [String: [String]] = [:] { didSet { if !loading { changed() } } }
    /// One chosen line per psyche question ("" = unanswered), index-aligned with `questions`.
    @Published var psyche: [String] = Array(repeating: "", count: 5) { didSet { if !loading { changed() } } }
    @Published var tease = "light" { didSet { if !loading { changed() } } }
    @Published var spark = "off" { didSet { if !loading { changed() } } }
    @Published var nudges = "gentle" { didSet { if !loading { changed() } } }
    @Published var checkin = true { didSet { if !loading { changed() } } }
    @Published var adultConfirmed = false { didSet { if !loading { changed() } } }
    @Published var adult = false
    @Published var localOnly = false
    @Published var lines: [ProfileLine] = []
    @Published var threads: [OpenThread] = []
    @Published var stats: [String: Any] = [:]
    @Published var note = ""
    @Published var busy = false

    init(base: URL) { api = LocalAPI(base: base) }
    /// Saves a moment after the last change (the user never has to find a Save button).
    private var saveTask: Task<Void, Never>?
    @Published var dirty = false
    func changed() {
        dirty = true
        saveTask?.cancel()
        saveTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 900_000_000)
            guard !Task.isCancelled, let self else { return }
            self.save()
        }
    }
    /// Leaving the screen: save now.
    func flush() { if dirty { saveTask?.cancel(); save() } }

    static let categories: [(id: String, title: String, options: [String])] = [
        ("activity", "Activity", ["running", "cycling", "motorbike", "climbing", "hiking", "gym", "swimming", "yoga", "football", "skiing", "sailing", "chess"]),
        ("music", "Music", ["rock", "jazz", "techno", "classical", "hip-hop", "indie", "metal", "pop", "folk", "vinyl", "live gigs"]),
        ("food", "Food & drink", ["italian", "asian", "vegetarian", "bbq", "sushi", "baking", "coffee", "wine", "craft beer", "spicy", "street food"]),
        ("humour", "Humour", ["dry", "dark", "silly", "sarcastic", "puns", "gentle"]),
        ("places", "Places", ["cafés", "mountains", "lakes", "cities", "museums", "concerts", "nature", "markets", "road trips"]),
    ]
    /// Five questions, three answers each; the answer text becomes a PSYCHE line.
    static let questions: [(q: String, a: [(label: String, line: String)])] = [
        ("When something goes wrong, what do you want from a friend first?", [("Fix it with me", "wants a fix first when things go wrong"), ("Hear me out", "wants to be heard before advice"), ("Make me laugh", "wants a joke before anything else when things go wrong")]),
        ("Advice?", [("Tell me straight", "takes advice straight, no cushioning"), ("Ask me first", "wants to be asked before advice is given"), ("Only if I ask", "wants advice only when they ask for it")]),
        ("Being teased?", [("Love it", "enjoys being teased"), ("A little", "takes a little teasing"), ("Not really", "doesn't enjoy being teased")]),
        ("Your energy comes from…", [("Quiet, one-on-one", "recharges alone or one-on-one; warms up slowly"), ("Depends on the day", "mixed: sometimes people, sometimes quiet"), ("People and plans", "recharges with people and plans")]),
        ("Plans?", [("Spontaneous", "prefers spontaneous plans"), ("A loose plan", "likes a loose plan with room to change"), ("Everything scheduled", "likes things scheduled and settled")]),
    ]

    func load() async {
        guard let j = await api.get("api/friend") else { note = "Couldn't reach Sam's service"; return }
        loading = true; defer { loading = false }
        let s = j["settings"] as? [String: Any] ?? [:]
        name = s["name"] as? String ?? ""
        home = s["home"] as? String ?? ""
        if let b = s["birthday"] as? String, let d = Self.df.date(from: b) { birthday = d; hasBirthday = true } else { hasBirthday = false }
        tastes = (s["tastes"] as? [String: [String]]) ?? [:]
        let saved = s["psyche"] as? [String] ?? []
        psyche = Self.questions.map { q in saved.first { p in q.a.contains { $0.line == p } } ?? "" }
        tease = s["tease"] as? String ?? "light"; spark = s["spark"] as? String ?? "off"; nudges = s["nudges"] as? String ?? "gentle"
        checkin = s["checkin"] as? Bool ?? true; adultConfirmed = s["adult_confirmed"] as? Bool ?? false
        adult = j["adult"] as? Bool ?? false; localOnly = j["local_only"] as? Bool ?? false
        lines = ((j["lines"] as? [[String: Any]]) ?? []).compactMap { l in (l["text"] as? String).map { ProfileLine(text: $0, source: l["source"] as? String ?? "") } }
        stats = j["stats"] as? [String: Any] ?? [:]
        threads = ((j["threads"] as? [[String: Any]]) ?? []).compactMap { t in
            guard let id = t["id"] as? Int, let text = t["text"] as? String else { return nil }
            return OpenThread(id: id, text: text, due: (t["due"] as? String).flatMap { Self.df.date(from: $0) }, offered: (t["status"] as? String) == "offered")
        }.sorted { ($0.due ?? .distantFuture) < ($1.due ?? .distantFuture) }
    }

    static let df: DateFormatter = { let f = DateFormatter(); f.dateFormat = "yyyy-MM-dd"; f.locale = Locale(identifier: "en_US_POSIX"); return f }()

    func save() {
        busy = true; note = ""
        let body: [String: Any] = ["name": name, "home": home, "birthday": hasBirthday ? Self.df.string(from: birthday) : "", "tastes": tastes, "psyche": psyche.filter { !$0.isEmpty },
                                   "tease": tease, "spark": spark, "nudges": nudges, "checkin": checkin, "adult_confirmed": adultConfirmed]
        Task {
            let ok = await api.send("api/friend", method: "POST", body: body)
            await load()
            busy = false
            dirty = !ok
            note = ok ? "Saved" : "Couldn't save: Sam's service didn't answer (is it running?)"
        }
    }

    func forget(_ line: ProfileLine) {
        Task { _ = await api.send("api/friend/forget", method: "POST", body: ["line": line.text]); await load() }
    }

    func drop(_ t: OpenThread) {
        Task { _ = await api.send("api/friend/forget", method: "POST", body: ["thread": t.id]); await load() }
    }

    func checkInNow() {
        busy = true; note = "Thinking about it… (this takes up to a minute)"
        Task {
            let r = await api.call("api/friend/checkin", method: "POST", body: [:], timeout: 120)
            await load(); busy = false
            guard let r else { note = "Couldn't run it: Sam's service didn't answer. Is Sam running?"; return }
            if let m = r["message"] as? String, r["ok"] as? Bool == false { note = "Couldn't run it: \(m)"; return }
            let items = (r["items"] as? [[String: Any]]) ?? []
            if let say = items.first?["say"] as? String, !say.isEmpty {
                note = "Sam has something: “\(say)” It comes up when the island next opens."
            } else {
                note = "Nothing worth bringing up right now. Sam only speaks up when something fits."
            }
        }
    }

    func toggle(_ cat: String, _ item: String) {
        var list = tastes[cat] ?? []
        if let i = list.firstIndex(of: item) { list.remove(at: i) } else { list.append(item) }
        tastes[cat] = list
    }
}

/// A tappable word badge.
struct TasteChip: View {
    let text: String; let on: Bool; let action: () -> Void
    var body: some View {
        Button(action: action) {
            Text(text).font(Theme.caption).foregroundStyle(on ? Color.black : Theme.text2)
                .padding(.horizontal, 10).padding(.vertical, 5)
                .background(Capsule().fill(on ? Theme.accent : Theme.fill))
                .overlay(Capsule().stroke(on ? Color.clear : Theme.strokeStrong))
        }
        .buttonStyle(.plain).accessibilityAddTraits(on ? .isSelected : [])
    }
}

/// Wraps chips onto as many rows as needed.
struct ChipRows: View {
    let items: [String]; let on: (String) -> Bool; let tap: (String) -> Void
    var body: some View {
        var x: CGFloat = 0, y: CGFloat = 0
        return GeometryReader { g in
            ZStack(alignment: .topLeading) {
                ForEach(items, id: \.self) { item in
                    TasteChip(text: item, on: on(item)) { tap(item) }
                        .padding(.trailing, 6).padding(.bottom, 6)
                        .alignmentGuide(.leading) { d in
                            if abs(x - d.width) > g.size.width { x = 0; y -= d.height }
                            let r = x; if item == items.last { x = 0 } else { x -= d.width }
                            return r
                        }
                        .alignmentGuide(.top) { _ in let r = y; if item == items.last { y = 0 }; return r }
                }
            }
        }
        .frame(height: CGFloat((items.count / 6 + 1)) * 30)
    }
}

/// Onboarding step: who you are and how Sam should be with you.
struct AboutYouView: View {
    @ObservedObject var f: FriendStore
    @State private var custom: [String: String] = [:]
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                Text("About you").font(Theme.display).tracking(-0.4).foregroundStyle(Theme.text)
                Text("Optional, and all of it stays on this Mac. It lets Sam talk like someone who knows you and notice things a friend would: a birthday coming up, a place that fits your taste. Every line is visible and deletable under You in the overview.")
                    .font(Theme.body).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 12) {
                    TextField("Your name", text: $f.name).textFieldStyle(.roundedBorder).frame(maxWidth: 180).accessibilityLabel("Your name")
                    TextField("Where you live (district, city)", text: $f.home).textFieldStyle(.roundedBorder).frame(maxWidth: 240).accessibilityLabel("Where you live")
                    Toggle("Birthday", isOn: $f.hasBirthday).toggleStyle(.switch)
                    if f.hasBirthday { DatePicker("", selection: $f.birthday, in: ...Date(), displayedComponents: .date).labelsHidden().accessibilityLabel("Birthday") }
                }
                ForEach(FriendStore.categories, id: \.id) { c in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(c.title).font(Theme.title).foregroundStyle(Theme.text)
                        ChipRows(items: c.options + (f.tastes[c.id] ?? []).filter { !c.options.contains($0) }, on: { (f.tastes[c.id] ?? []).contains($0) }, tap: { f.toggle(c.id, $0) })
                        HStack {
                            TextField("Add your own", text: Binding(get: { custom[c.id] ?? "" }, set: { custom[c.id] = $0 })).textFieldStyle(.roundedBorder).frame(maxWidth: 200)
                                .onSubmit { let t = (custom[c.id] ?? "").trimmingCharacters(in: .whitespaces); if !t.isEmpty { f.toggle(c.id, t.lowercased()); custom[c.id] = "" } }
                        }
                    }
                }
                Text("Five quick ones").font(Theme.title).foregroundStyle(Theme.text)
                Text("A guess at how you tick, kept as a hypothesis Sam can be wrong about.").font(Theme.caption).foregroundStyle(Theme.text3)
                ForEach(Array(FriendStore.questions.enumerated()), id: \.offset) { i, q in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(q.q).font(Theme.body).foregroundStyle(Theme.text2)
                        Picker("", selection: $f.psyche[i]) {
                            Text("—").tag("")
                            ForEach(q.a, id: \.line) { a in Text(a.label).tag(a.line) }
                        }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 520)
                    }
                }
                FriendSettingsView(f: f)
                HStack(spacing: 10) {
                    Button(f.busy ? "Saving…" : "Save now") { f.save() }.samButton().disabled(f.busy)
                    Text(f.busy ? "Saving…" : (f.dirty ? "Saving in a moment…" : (f.note.isEmpty ? "Everything here saves by itself." : f.note))).font(Theme.caption).foregroundStyle(f.note.hasPrefix("Couldn't") ? Theme.warn : Theme.text3)
                }
            }
        }
        .scrollIndicators(.never)
        .task { await f.load() }
        .onDisappear { f.flush() }
    }
}

/// Tease, spark, nudges, the daily check-in.
struct FriendSettingsView: View {
    @ObservedObject var f: FriendStore
    var titled = true
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if titled { Text("How Sam is with you").font(Theme.title).foregroundStyle(Theme.text) }
            if f.localOnly {
                Text("With a local model these stay off: it can't carry a personality reliably (lab: 4 of 10). Pick a cloud provider under AI provider to turn them on.").font(Theme.caption).foregroundStyle(Theme.warn).fixedSize(horizontal: false, vertical: true)
            }
            row("Teasing", Picker("", selection: $f.tease) { Text("Off").tag("off"); Text("Light").tag("light"); Text("Dark").tag("dark") })
            row("Spark", Picker("", selection: $f.spark) { Text("Off").tag("off"); Text("Playful").tag("playful"); Text("Flirty").tag("flirty").disabled(!(f.adult && f.adultConfirmed)) })
            if f.spark == "flirty" || !f.adult || !f.adultConfirmed {
                Toggle("I'm over 18 and I'd like Sam to flirt back a little (never first, never explicit; it needs your birthday above)", isOn: $f.adultConfirmed).toggleStyle(.switch).font(Theme.caption).foregroundStyle(Theme.text3)
            }
            row("Nudges", Picker("", selection: $f.nudges) { Text("Off").tag("off"); Text("Gentle").tag("gentle") })
            Toggle("Daily check-in: once a day, at a quiet moment, Sam may bring up one small thing (a birthday coming up, something you asked about). It asks, never acts on its own, and backs off when you ignore it.", isOn: $f.checkin)
                .toggleStyle(.switch).font(Theme.caption).foregroundStyle(Theme.text2)
        }
        .disabled(f.localOnly)
    }
    private func row(_ label: String, _ picker: some View) -> some View {
        HStack(spacing: 8) { Text(label).font(Theme.body).foregroundStyle(Theme.text2).frame(width: 70, alignment: .leading); picker.pickerStyle(.segmented).labelsHidden().fixedSize(); Spacer(minLength: 0) }
    }
}

/// One known line, split into its kind (the card prefix) and readable text.
struct KnownLine: Identifiable {
    let line: ProfileLine
    let kind: String
    let text: String
    /// PSYCHE lines carry how sure Sam is ("(0.7, from the onboarding)").
    var confidence: Double?
    var note: String?
    var id: String { line.id }
    var learned: Bool { line.source != "you" }

    init(_ l: ProfileLine) {
        line = l
        var k = "OTHER", t = l.text
        if let r = l.text.range(of: #"^[A-Z]+:\s*"#, options: .regularExpression) {
            k = String(l.text[r]).trimmingCharacters(in: CharacterSet(charactersIn: ": "))
            t = String(l.text[r.upperBound...])
        }
        if let r = t.range(of: #"\s*\(([01](?:\.\d+)?)(?:,\s*([^)]*))?\)\s*$"#, options: .regularExpression) {
            let inner = t[r].trimmingCharacters(in: CharacterSet(charactersIn: " ()"))
            let parts = inner.split(separator: ",", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            if let c = Double(parts.first ?? "") { confidence = c; note = parts.count > 1 ? parts[1] : nil; t = String(t[..<r.lowerBound]) }
        }
        // HOME carries an instruction for the model in brackets; the person only needs the place.
        if k == "HOME", let r = t.range(of: #"\s*\(.*$"#, options: .regularExpression) { t = String(t[..<r.lowerBound]) }
        kind = k
        text = t.prefix(1).uppercased() + t.dropFirst()
    }
}

/// The You tab's sections, in reading order.
enum YouSection: String, CaseIterable, Identifiable {
    case taste, upcoming, people, tick, habits, asks, about, checkins, manner
    var id: String { rawValue }
    var title: String {
        switch self {
        case .taste: "Your taste"; case .upcoming: "Coming up"; case .people: "People & dates"; case .tick: "How you tick"; case .habits: "Habits"
        case .asks: "How you want Sam to be"; case .about: "About you"; case .checkins: "Check-ins"; case .manner: "How Sam is with you"
        }
    }
    var symbol: String {
        switch self {
        case .taste: "heart.fill"; case .upcoming: "calendar.badge.clock"; case .people: "person.2.fill"; case .tick: "brain.head.profile"; case .habits: "repeat"
        case .asks: "text.bubble.fill"; case .about: "person.text.rectangle.fill"; case .checkins: "sun.horizon.fill"; case .manner: "slider.horizontal.3"
        }
    }
    var tone: Color {
        switch self {
        case .taste: Color(red: 1.0, green: 0.55, blue: 0.45); case .upcoming: Color(red: 0.55, green: 0.85, blue: 1.0); case .people: Color(red: 1.0, green: 0.62, blue: 0.80); case .tick: Theme.thinkB
        case .habits: Color(red: 0.40, green: 0.85, blue: 0.75); case .asks: Theme.accent; case .about: Color(red: 0.55, green: 0.75, blue: 1.0)
        case .checkins: Color(red: 1.0, green: 0.75, blue: 0.35); case .manner: Theme.text2
        }
    }
    var blurb: String {
        switch self {
        case .taste: "What you like. Sam picks places, music and plans from this."
        case .upcoming: "Things you mentioned that are still ahead. When the day comes, Sam asks how it went, once."
        case .people: "Who matters to you, and the days not to forget."
        case .tick: "Guesses Sam holds loosely. It can be wrong; delete what doesn't fit."
        case .habits: "Routines Sam has noticed."
        case .asks: "How you've asked Sam to behave."
        case .about: "Other things Sam has picked up."
        case .checkins: "Once a day, at a quiet moment, Sam may bring up one small thing."
        case .manner: "Teasing, spark and nudges. Changes save by themselves."
        }
    }
    var kinds: [String] {
        switch self {
        case .taste: ["TASTE"]; case .people: ["RELATIONSHIP", "DATE"]; case .tick: ["PSYCHE"]; case .habits: ["HABIT"]
        case .asks: ["INSTRUCTION"]; case .about: ["IDENTITY", "ATTRIBUTE", "HOME", "OTHER"]; default: []
        }
    }
    static let knowledge: [YouSection] = [.people, .tick, .habits, .asks, .about]
    static let categorySymbols = ["activity": "figure.run", "music": "music.note", "food": "fork.knife", "humour": "face.smiling", "places": "mappin.and.ellipse"]
}

extension FriendStore {
    /// Lines for the cards: what the hero already shows (name, home, own birthday) and the
    /// onboarding tastes (drawn as chips) are left out.
    var known: [KnownLine] {
        lines.map(KnownLine.init).filter { k in
            guard !k.learned else { return true }
            if ["IDENTITY", "HOME", "TASTE"].contains(k.kind) { return false }
            if k.kind == "DATE" && k.text.lowercased().contains("their own birthday") { return false }
            return true
        }
    }
    func known(_ s: YouSection) -> [KnownLine] { known.filter { s.kinds.contains($0.kind) } }
    var tasteCount: Int { tastes.values.reduce(0) { $0 + $1.count } + known(.taste).count }
    func count(_ s: YouSection) -> Int { s == .taste ? tasteCount : s == .upcoming ? threads.count : known(s).count }
    var age: Int? { hasBirthday ? Calendar.current.dateComponents([.year], from: birthday, to: Date()).year : nil }
    /// Days until the next birthday (0 = today).
    var birthdayIn: Int? {
        guard hasBirthday else { return nil }
        let cal = Calendar.current, today = cal.startOfDay(for: Date())
        var c = cal.dateComponents([.month, .day], from: birthday); c.year = cal.component(.year, from: today)
        guard var next = cal.date(from: c) else { return nil }
        if next < today { c.year! += 1; next = cal.date(from: c) ?? next }
        return cal.dateComponents([.day], from: today, to: next).day
    }
}

/// Initial on the dawn gradient.
struct YouAvatar: View {
    let name: String; var size: CGFloat = 64
    var body: some View {
        ZStack {
            Circle().fill(Dawn.gradient)
            if let c = name.trimmingCharacters(in: .whitespaces).first {
                Text(String(c).uppercased()).font(.system(size: size * 0.42, weight: .semibold, design: .rounded)).foregroundStyle(.white)
            } else {
                Image(systemName: "person.fill").font(.system(size: size * 0.4)).foregroundStyle(.white.opacity(0.9))
            }
        }
        .frame(width: size, height: size)
        .shadow(color: Dawn.colors[1].opacity(0.35), radius: size * 0.2, y: size * 0.06)
        .accessibilityHidden(true)
    }
}

/// Sidebar of the You tab: who, at a glance, and an index into the cards.
struct YouSidebar: View {
    @ObservedObject var f: FriendStore
    @ObservedObject var layout: OverviewLayout
    @State private var hovered: YouSection?
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                HStack(spacing: 12) {
                    YouAvatar(name: f.name, size: 44)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(f.name.isEmpty ? "You" : f.name).font(Theme.title).foregroundStyle(Theme.text)
                        Text(f.home.isEmpty ? "\(f.lines.count) things Sam knows" : f.home).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                    }
                }
                .padding(12).frame(maxWidth: .infinity, alignment: .leading).samCard()
                VStack(alignment: .leading, spacing: 2) {
                    Text("SECTIONS").font(Theme.overline).foregroundStyle(Theme.text3).padding(.leading, 8).padding(.bottom, 4)
                    ForEach(YouSection.allCases) { s in
                        Button { layout.youSection = s } label: {
                            HStack(spacing: 10) {
                                IconTile(symbol: s.symbol, size: 24, tone: s.tone)
                                Text(s.title).font(Theme.body).foregroundStyle(Theme.text)
                                Spacer()
                                if !s.kinds.isEmpty || s == .upcoming {
                                    Text("\(f.count(s))").font(Theme.caption.monospacedDigit()).foregroundStyle(Theme.text3)
                                }
                            }
                            .padding(.horizontal, 8).padding(.vertical, 6)
                            .background(RoundedRectangle(cornerRadius: Theme.chipRadius, style: .continuous).fill(hovered == s ? Theme.fillHover : .clear))
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .onHover { hovered = $0 ? s : (hovered == s ? nil : hovered) }
                        .accessibilityLabel("\(s.title), \(f.count(s)) items")
                    }
                }
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: "lock.fill").font(.system(size: 10)).foregroundStyle(Theme.text3).padding(.top, 2)
                    Text("All of this stays on this Mac. Hover a line to forget it; it won't come back.").font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 8)
            }
        }
        .scrollIndicators(.never)
        .task { await f.load() }
    }
}

/// The You tab's main pane: a profile header, then one card per kind of thing Sam knows.
struct YouDetail: View {
    @ObservedObject var f: FriendStore
    @ObservedObject var layout: OverviewLayout
    @State private var editing = false

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    hero
                    tasteCard.id(YouSection.taste)
                    if f.threads.isEmpty { Color.clear.frame(height: 0).id(YouSection.upcoming) } else { upcomingCard.id(YouSection.upcoming) }
                    let filled = YouSection.knowledge.filter { !f.known($0).isEmpty }
                    if filled.isEmpty && f.known(.taste).isEmpty && f.tastes.values.allSatisfy(\.isEmpty) {
                        emptyCard
                    }
                    LazyVGrid(columns: [GridItem(.flexible(), spacing: 14, alignment: .top), GridItem(.flexible(), spacing: 14, alignment: .top)], alignment: .leading, spacing: 14) {
                        ForEach(filled) { s in knowledgeCard(s).id(s) }
                    }
                    ForEach(YouSection.knowledge.filter { f.known($0).isEmpty }) { s in Color.clear.frame(height: 0).id(s) }
                    HStack(alignment: .top, spacing: 14) {
                        checkinCard.id(YouSection.checkins)
                        mannerCard.id(YouSection.manner)
                    }
                    footer
                }
                .padding(24)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .scrollIndicators(.never)
            .onChange(of: layout.youSection) { _, s in
                guard let s else { return }
                // An empty section has no card: land on the first card instead of nowhere.
                let target = ((YouSection.knowledge.contains(s) && f.known(s).isEmpty) || (s == .upcoming && f.threads.isEmpty)) ? YouSection.taste : s
                withAnimation(.smooth(duration: 0.35)) { proxy.scrollTo(target, anchor: .top) }
                layout.youSection = nil
            }
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
        .onDisappear { f.flush() }
    }

    // MARK: Header

    private var hero: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .center, spacing: 18) {
                YouAvatar(name: f.name, size: 72)
                VStack(alignment: .leading, spacing: 8) {
                    Text(f.name.isEmpty ? "You" : f.name).font(Theme.h1).foregroundStyle(Theme.text)
                    HStack(spacing: 6) {
                        if let a = f.age { Chip(icon: "person", text: "\(a)") }
                        if !f.home.isEmpty { Chip(icon: "mappin", text: f.home) }
                        if f.hasBirthday, let d = f.birthdayIn {
                            Chip(icon: "gift", text: d == 0 ? "Birthday today" : "\(f.birthday.formatted(.dateTime.day().month(.wide))) · in \(d) day\(d == 1 ? "" : "s")")
                        }
                        if f.name.isEmpty && f.home.isEmpty && !f.hasBirthday {
                            Text("Add your name, where you live and your birthday, and Sam stops asking.").font(Theme.caption).foregroundStyle(Theme.text3)
                        }
                    }
                }
                Spacer(minLength: 8)
                Button { withAnimation(.smooth(duration: 0.25)) { editing.toggle() } } label: {
                    Label(editing ? "Done" : "Edit", systemImage: editing ? "checkmark" : "pencil")
                }
                .samButton().controlSize(.small)
            }
            if editing {
                HStack(spacing: 10) {
                    TextField("Your name", text: $f.name).textFieldStyle(.roundedBorder).frame(maxWidth: 180).accessibilityLabel("Your name")
                    TextField("Where you live (district, city)", text: $f.home).textFieldStyle(.roundedBorder).frame(maxWidth: 240).accessibilityLabel("Where you live")
                    Toggle("Birthday", isOn: $f.hasBirthday).toggleStyle(.switch).controlSize(.small)
                    if f.hasBirthday { DatePicker("", selection: $f.birthday, in: ...Date(), displayedComponents: .date).labelsHidden().accessibilityLabel("Birthday") }
                }
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
            HStack(spacing: 10) {
                stat("\(f.lines.count)", "things Sam knows", "sparkles")
                stat("\(f.lines.filter { $0.source == "you" }.count)", "you told Sam", "person.fill")
                stat("\(f.lines.filter { $0.source != "you" }.count)", "learned from talking", "bubble.left.and.bubble.right.fill")
            }
        }
        .padding(20)
        .background(
            LinearGradient(colors: [Dawn.colors[0].opacity(0.22), Dawn.colors[1].opacity(0.10), .clear], startPoint: .topLeading, endPoint: .bottomTrailing)
        )
        .samCard(radius: Theme.paneRadius - 6)
    }

    private func stat(_ n: String, _ label: String, _ symbol: String) -> some View {
        HStack(spacing: 10) {
            Image(systemName: symbol).font(.system(size: 12, weight: .semibold)).foregroundStyle(Theme.text3)
            VStack(alignment: .leading, spacing: 0) {
                Text(n).font(Theme.stat).foregroundStyle(Theme.text)
                Text(label).font(Theme.caption).foregroundStyle(Theme.text3)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12).padding(.vertical, 10)
        .frame(maxWidth: .infinity)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Color.black.opacity(0.18)))
    }

    // MARK: Cards

    private func cardHeader(_ s: YouSection, count: Int? = nil) -> some View {
        HStack(alignment: .top, spacing: 10) {
            IconTile(symbol: s.symbol, size: 30, tone: s.tone)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(s.title).font(Theme.title).foregroundStyle(Theme.text)
                    if let count, count > 0 { Text("\(count)").font(Theme.micro.monospacedDigit()).foregroundStyle(Theme.text3).padding(.horizontal, 6).padding(.vertical, 2).background(Capsule().fill(Theme.fill)) }
                }
                Text(s.blurb).font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private var tasteCard: some View {
        let cats = FriendStore.categories.map { ($0.id, $0.title, $0.options) } + f.tastes.keys.filter { k in !FriendStore.categories.contains { $0.id == k } }.sorted().map { ($0, $0.capitalized, [String]()) }
        return VStack(alignment: .leading, spacing: 14) {
            cardHeader(.taste, count: f.tasteCount)
            VStack(alignment: .leading, spacing: 10) {
                ForEach(cats, id: \.0) { id, title, options in
                    let mine = f.tastes[id] ?? []
                    HStack(alignment: .firstTextBaseline, spacing: 12) {
                        Label(title, systemImage: YouSection.categorySymbols[id] ?? "tag").font(Theme.caption).foregroundStyle(Theme.text2)
                            .frame(width: 112, alignment: .leading)
                        FlowRow(spacing: 6) {
                            ForEach(mine, id: \.self) { item in
                                RemovableChip(text: item, tone: YouSection.taste.tone) { f.toggle(id, item) }
                            }
                            let rest = options.filter { !mine.contains($0) }
                            if !rest.isEmpty {
                                Menu {
                                    ForEach(rest, id: \.self) { o in Button(o) { f.toggle(id, o) } }
                                } label: {
                                    Image(systemName: "plus").font(.system(size: 9, weight: .bold)).foregroundStyle(Theme.text3)
                                        .frame(width: 22, height: 22).background(Circle().strokeBorder(Theme.strokeStrong, style: StrokeStyle(lineWidth: 1, dash: [3, 2])))
                                }
                                .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden).fixedSize()
                                .accessibilityLabel("Add to \(title)")
                            }
                        }
                    }
                }
            }
            let learned = f.known(.taste)
            if !learned.isEmpty {
                Divider().overlay(Theme.stroke)
                Text("SAM NOTICED").font(Theme.overline).foregroundStyle(Theme.text3)
                VStack(alignment: .leading, spacing: 2) { ForEach(learned) { KnownRow(k: $0, f: f) } }
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .samCard()
    }

    private func knowledgeCard(_ s: YouSection) -> some View {
        let items = f.known(s)
        return VStack(alignment: .leading, spacing: 12) {
            cardHeader(s, count: items.count)
            VStack(alignment: .leading, spacing: 2) { ForEach(items) { KnownRow(k: $0, f: f, tone: s.tone) } }
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .samCard()
    }

    private var emptyCard: some View {
        HStack(spacing: 14) {
            Image(systemName: "bubble.left.and.text.bubble.right").font(.system(size: 24)).foregroundStyle(Theme.text3)
            VStack(alignment: .leading, spacing: 3) {
                Text("Sam is still getting to know you").font(Theme.title).foregroundStyle(Theme.text)
                Text("Just talk. People, routines and the way you like things land here as Sam picks them up, each one yours to keep or delete.").font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(18).frame(maxWidth: .infinity, alignment: .leading).samCard()
    }

    private var upcomingCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            cardHeader(.upcoming, count: f.threads.count)
            VStack(alignment: .leading, spacing: 2) {
                ForEach(f.threads) { t in ThreadRow(t: t, f: f, tone: YouSection.upcoming.tone) }
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .samCard()
    }

    private var checkinCard: some View {
        let acc = f.stats["accepted"] as? Int ?? 0, ign = f.stats["ignored"] as? Int ?? 0, drop = f.stats["dropped"] as? Int ?? 0, cad = f.stats["cadence_days"] as? Int ?? 1
        return VStack(alignment: .leading, spacing: 14) {
            cardHeader(.checkins)
            HStack(spacing: 8) {
                miniStat(acc, "taken up", Theme.good)
                miniStat(ign, "ignored", Theme.text2)
                miniStat(drop, "held back", Theme.warn)
            }
            if let last = f.stats["last"] as? [String: Any], let say = last["say"] as? String, !say.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("LAST TIME").font(Theme.overline).foregroundStyle(Theme.text3)
                    Text("“\(say)”").font(Theme.body).italic().foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
                    if let st = last["status"] as? String, !st.isEmpty { Text(st.capitalized).font(Theme.caption).foregroundStyle(Theme.text3) }
                }
                .padding(12).frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Color.black.opacity(0.18)))
            }
            HStack(spacing: 10) {
                Button(f.busy ? "Thinking…" : "Check in now") { f.checkInNow() }.samButton().controlSize(.small).disabled(f.busy || f.localOnly || !f.checkin)
                Text(!f.checkin ? "The check-in is off" : "Every \(cad == 1 ? "day" : "\(cad) days")\(cad > 1 ? ": you've been ignoring it, so Sam backs off" : "")")
                    .font(Theme.caption).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
            }
            Text("Held back = a check-in that didn't pass Sam's own fact check.").font(Theme.micro).foregroundStyle(Theme.text3)
        }
        .padding(18)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .samCard()
    }

    private func miniStat(_ n: Int, _ label: String, _ tone: Color) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("\(n)").font(Theme.stat).foregroundStyle(n > 0 ? tone : Theme.text3)
            Text(label).font(Theme.caption).foregroundStyle(Theme.text3)
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Color.black.opacity(0.18)))
    }

    private var mannerCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            cardHeader(.manner)
            FriendSettingsView(f: f, titled: false)
        }
        .padding(18)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .samCard()
    }

    private var footer: some View {
        HStack(spacing: 6) {
            if f.busy { ProgressView().controlSize(.mini) }
            Text(f.busy ? "Saving…" : (f.dirty ? "Saving in a moment…" : (f.note.isEmpty ? "Everything here saves by itself." : f.note)))
                .font(Theme.caption).foregroundStyle(f.note.hasPrefix("Couldn't") ? Theme.warn : Theme.text3)
        }
        .padding(.horizontal, 4)
    }
}

/// One thing Sam knows: readable text, where it came from, how sure Sam is, and forget on hover.
struct KnownRow: View {
    let k: KnownLine
    @ObservedObject var f: FriendStore
    var tone: Color = Theme.accent
    @State private var hover = false
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Circle().fill(k.learned ? tone : Theme.text3).frame(width: 6, height: 6).padding(.top, 5)
                .help(k.learned ? "Learned from talking" : "You told Sam")
            VStack(alignment: .leading, spacing: 4) {
                Text(k.text).font(Theme.body).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                if let c = k.confidence {
                    HStack(spacing: 6) {
                        Capsule().fill(Theme.fill).frame(width: 60, height: 4)
                            .overlay(alignment: .leading) { Capsule().fill(tone).frame(width: 60 * min(max(c, 0), 1), height: 4) }
                        Text("\(Int((c * 100).rounded()))% sure\(k.note.map { " · \($0)" } ?? "")").font(Theme.micro).foregroundStyle(Theme.text3)
                    }
                    .accessibilityLabel("Sam is \(Int((c * 100).rounded())) percent sure")
                } else if !k.learned {
                    Text("you told Sam").font(Theme.micro).foregroundStyle(Theme.text3)
                }
            }
            Spacer(minLength: 4)
            Button { f.forget(k.line) } label: { Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).frame(width: 20, height: 20).background(Circle().fill(Theme.fillHover)) }
                .buttonStyle(.plain).foregroundStyle(Theme.text2).opacity(hover ? 1 : 0)
                .help("Forget this").accessibilityLabel("Forget: \(k.text)")
        }
        .padding(.horizontal, 8).padding(.vertical, 7)
        .background(RoundedRectangle(cornerRadius: Theme.chipRadius, style: .continuous).fill(hover ? Theme.fill : .clear))
        .onHover { hover = $0 }
    }
}

/// One open thread: what, when, and an x so Sam won't ask about it.
struct ThreadRow: View {
    let t: OpenThread
    @ObservedObject var f: FriendStore
    var tone: Color
    @State private var hover = false
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Circle().fill(t.offered ? Theme.text3 : tone).frame(width: 6, height: 6).padding(.top, 5)
            VStack(alignment: .leading, spacing: 3) {
                Text(t.text).font(Theme.body).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                Text(when).font(Theme.micro).foregroundStyle(Theme.text3)
            }
            Spacer(minLength: 4)
            Button { f.drop(t) } label: { Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).frame(width: 20, height: 20).background(Circle().fill(Theme.fillHover)) }
                .buttonStyle(.plain).foregroundStyle(Theme.text2).opacity(hover ? 1 : 0)
                .help("Don't ask about this").accessibilityLabel("Don't ask about: \(t.text)")
        }
        .padding(.horizontal, 8).padding(.vertical, 7)
        .background(RoundedRectangle(cornerRadius: Theme.chipRadius, style: .continuous).fill(hover ? Theme.fill : .clear))
        .onHover { hover = $0 }
    }
    private var when: String {
        if t.offered { return "Sam asked about it" }
        guard let d = t.due else { return "no day said · Sam asks in a few days" }
        let cal = Calendar.current, n = cal.dateComponents([.day], from: cal.startOfDay(for: Date()), to: cal.startOfDay(for: d)).day ?? 0
        let day = d.formatted(.dateTime.weekday(.wide).day().month(.abbreviated))
        return n == 0 ? "today" : n == 1 ? "tomorrow · \(day)" : n == -1 ? "yesterday" : n < 0 ? "\(day) · Sam asks how it went" : "\(day) · in \(n) days"
    }
}

/// A chosen taste: tinted, with a small x to drop it.
struct RemovableChip: View {
    let text: String; let tone: Color; let remove: () -> Void
    var body: some View {
        HStack(spacing: 5) {
            Text(text).font(Theme.caption).foregroundStyle(Theme.text)
            Button(action: remove) { Image(systemName: "xmark").font(.system(size: 7, weight: .bold)).foregroundStyle(Theme.text2) }
                .buttonStyle(.plain).accessibilityLabel("Remove \(text)")
        }
        .padding(.leading, 10).padding(.trailing, 8).padding(.vertical, 4)
        .background(Capsule().fill(tone.opacity(0.16)))
        .overlay(Capsule().strokeBorder(tone.opacity(0.30)))
    }
}

/// Lays children out left to right, wrapping onto new rows.
struct FlowRow: Layout {
    var spacing: CGFloat = 6
    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let w = proposal.width ?? .infinity
        var x: CGFloat = 0, y: CGFloat = 0, row: CGFloat = 0, widest: CGFloat = 0
        for s in subviews {
            let d = s.sizeThatFits(.unspecified)
            if x > 0 && x + d.width > w { y += row + spacing; x = 0; row = 0 }
            x += d.width + spacing; row = max(row, d.height); widest = max(widest, x - spacing)
        }
        return CGSize(width: proposal.width ?? widest, height: y + row)
    }
    func placeSubviews(in b: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = b.minX, y = b.minY, row: CGFloat = 0
        for s in subviews {
            let d = s.sizeThatFits(.unspecified)
            if x > b.minX && x + d.width > b.maxX { y += row + spacing; x = b.minX; row = 0 }
            s.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(d))
            x += d.width + spacing; row = max(row, d.height)
        }
    }
}
