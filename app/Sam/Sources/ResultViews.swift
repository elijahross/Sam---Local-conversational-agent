// Typed results: what a quick task produced, drawn from its data instead of one sentence.
// The server attaches a `view` to the task ({"type": "music" | "weather" | "reminder" | "event"});
// the island card shows the compact form and the overview the large one (2026-10-15: the full
// window showed "playing on YouTube: …" or "reminder added: {json}" as plain text).
import AppKit
import SwiftUI

/// The dawn gradient of Sam's identity (orb, art tiles).
enum Dawn {
    static let colors = [Color(red: 0.37, green: 0.36, blue: 0.90), Color(red: 0.71, green: 0.49, blue: 1.0), Color(red: 1.0, green: 0.60, blue: 0.42)]
    static var gradient: LinearGradient { LinearGradient(colors: colors, startPoint: .topLeading, endPoint: .bottomTrailing) }
}

/// Real app icons for a result's source: the player app, or the browser a video plays in.
enum SourceIcon {
    static func app(bundle: String) -> NSImage? {
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundle) else { return nil }
        return NSWorkspace.shared.icon(forFile: url.path)
    }
    static func app(named name: String) -> NSImage? {
        if !name.isEmpty, let ra = NSWorkspace.shared.runningApplications.first(where: { ($0.localizedName ?? "").caseInsensitiveCompare(name) == .orderedSame }), let u = ra.bundleURL {
            return NSWorkspace.shared.icon(forFile: u.path)
        }
        return nil
    }
    static func defaultBrowser() -> (NSImage, String)? {
        guard let u = NSWorkspace.shared.urlForApplication(toOpen: URL(string: "https://example.com")!) else { return nil }
        return (NSWorkspace.shared.icon(forFile: u.path), FileManager.default.displayName(atPath: u.path).replacingOccurrences(of: ".app", with: ""))
    }
    /// (icon, "where it plays") for a music source.
    static func music(service: String, app: String) -> (NSImage?, String) {
        switch service {
        case "spotify": return (Self.app(bundle: "com.spotify.client"), "Spotify")
        case "music": return (Self.app(bundle: "com.apple.Music"), "Apple Music")
        default:
            if let i = Self.app(named: app) { return (i, "YouTube · \(app)") }
            if let (i, n) = defaultBrowser() { return (i, "YouTube · \(n)") }
            return (nil, "YouTube")
        }
    }
}

struct TypedResultView: View {
    let view: [String: Any]
    var large = false
    var body: some View {
        switch view["type"] as? String ?? "" {
        case "music": NowPlayingView(view: view, large: large)
        case "weather": WeatherView(data: view["data"] as? [String: Any] ?? [:], large: large)
        case "reminder", "event": AddedView(view: view, large: large)
        case "agenda": AgendaView(view: view, large: large)
        case "todos": TodosView(items: view["items"] as? [[String: Any]] ?? [], large: large)
        case "files": FilesView(paths: view["paths"] as? [String] ?? [], large: large)
        default: EmptyView()
        }
    }
}

// MARK: - Now playing

struct NowPlayingView: View {
    let view: [String: Any]
    var large = false
    @State private var state: String = ""
    @State private var busy = false
    @State private var note: String?
    @Environment(\.accessibilityReduceMotion) private var reduce

    private var service: String { view["service"] as? String ?? "youtube" }
    private var title: String { (view["title"] as? String ?? "").replacingOccurrences(of: "playlist \"", with: "").replacingOccurrences(of: "\"", with: "") }
    private var isPlaylist: Bool { (view["title"] as? String ?? "").hasPrefix("playlist") || (view["url"] as? String ?? "").contains("list=PL") }
    private var playing: Bool { (state.isEmpty ? view["state"] as? String : state) == "playing" }

    var body: some View {
        let (icon, source) = SourceIcon.music(service: service, app: view["app"] as? String ?? "")
        let art: CGFloat = large ? 96 : 52
        VStack(alignment: .leading, spacing: large ? 16 : 10) {
            HStack(spacing: large ? 18 : 12) {
                ZStack(alignment: .bottomTrailing) {
                    RoundedRectangle(cornerRadius: art * 0.22, style: .continuous).fill(Dawn.gradient)
                        .frame(width: art, height: art)
                        .overlay(Image(systemName: isPlaylist ? "music.note.list" : "music.note").font(.system(size: art * 0.38, weight: .semibold)).foregroundStyle(.white.opacity(0.92)))
                    if let icon {
                        Image(nsImage: icon).resizable().frame(width: art * 0.42, height: art * 0.42).offset(x: art * 0.12, y: art * 0.12)
                            .shadow(color: .black.opacity(0.4), radius: 3, y: 1)
                    }
                }
                VStack(alignment: .leading, spacing: 3) {
                    Text(playing ? "NOW PLAYING" : "PAUSED").font(Theme.overline).tracking(0.8).foregroundStyle(Theme.text3)
                    Text(title.isEmpty ? "Music" : title).font(large ? Theme.h2 : Theme.title).foregroundStyle(Theme.text).lineLimit(large ? 3 : 1)
                    Text(source).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1)
                }
                Spacer(minLength: 6)
                Equalizer(active: playing && !reduce).frame(width: large ? 22 : 16, height: large ? 20 : 14)
                Button { send(playing ? "pause" : "resume") } label: {
                    Image(systemName: playing ? "pause.fill" : "play.fill").font(.system(size: large ? 17 : 13, weight: .bold))
                        .frame(width: large ? 44 : 32, height: large ? 44 : 32)
                        .foregroundStyle(.black).background(Circle().fill(.white))
                }
                .buttonStyle(.plain).disabled(busy).opacity(busy ? 0.5 : 1)
                .accessibilityLabel(playing ? "Pause" : "Play")
            }
            if large {
                HStack(spacing: 10) {
                    Button { send("next") } label: { Label("Next", systemImage: "forward.fill") }.buttonStyle(IslandButton(primary: false))
                    if let u = view["url"] as? String, let url = URL(string: u), u.hasPrefix("http") {
                        Button { NSWorkspace.shared.open(url) } label: { Label("Open in browser", systemImage: "arrow.up.forward") }.buttonStyle(IslandButton(primary: false))
                    }
                    Spacer()
                }
            }
            if let note { Text(note).font(Theme.caption).foregroundStyle(Theme.warn).lineLimit(2) }
        }
        .padding(large ? 18 : 0)
    }

    private func send(_ action: String) {
        busy = true; note = nil
        Task { @MainActor in
            var req = URLRequest(url: Remote.base.appendingPathComponent("api/music"), timeoutInterval: 45)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            LocalAuth.sign(&req)
            req.httpBody = try? JSONSerialization.data(withJSONObject: ["action": action])
            if let (d, _) = try? await URLSession.shared.data(for: req), let j = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any] {
                if let now = j["now"] as? [String: Any], let s = now["state"] as? String { state = s }
                else if action == "pause" { state = "paused" } else if action == "resume" { state = "playing" }
                if (j["ok"] as? Bool) != true { note = j["message"] as? String }
            } else {
                note = "Sam isn't reachable right now."
            }
            busy = false
        }
    }
}

/// Three bars that dance while music plays and rest when it doesn't.
struct Equalizer: View {
    var active: Bool
    var body: some View {
        TimelineView(.animation(minimumInterval: 1 / 30, paused: !active)) { ctx in
            let t = ctx.date.timeIntervalSinceReferenceDate
            HStack(alignment: .bottom, spacing: 2) {
                ForEach(0..<3, id: \.self) { i in
                    let h = active ? 0.35 + 0.65 * abs(sin(t * (5.2 + Double(i) * 1.3) + Double(i))) : 0.3
                    RoundedRectangle(cornerRadius: 1.5).fill(Theme.good).frame(maxWidth: .infinity).frame(maxHeight: .infinity, alignment: .bottom)
                        .scaleEffect(x: 1, y: h, anchor: .bottom)
                }
            }
        }
        .accessibilityHidden(true)
    }
}

// MARK: - Weather

struct WeatherView: View {
    let data: [String: Any]
    var large = false

    private func num(_ v: Any?) -> String { (v as? Double).map { String(format: "%.0f°", $0) } ?? (v as? Int).map { "\($0)°" } ?? "–" }
    private func dbl(_ v: Any?) -> Double { (v as? Double) ?? Double(v as? Int ?? 0) }
    static func symbol(_ sky: String) -> String {
        let s = sky.lowercased()
        if s.contains("thunder") { return "cloud.bolt.rain.fill" }
        if s.contains("snow") { return "cloud.snow.fill" }
        if s.contains("rain") || s.contains("shower") || s.contains("drizzle") { return "cloud.rain.fill" }
        if s.contains("fog") || s.contains("mist") { return "cloud.fog.fill" }
        if s.contains("partly") || s.contains("mainly") { return "cloud.sun.fill" }
        if s.contains("cloud") || s.contains("overcast") { return "cloud.fill" }
        return "sun.max.fill"
    }

    var body: some View {
        let now = data["now"] as? [String: Any] ?? [:]
        let days = (data["daily"] as? [[String: Any]]) ?? []
        let sky = now["sky"] as? String ?? ""
        let today = days.first ?? [:]
        VStack(alignment: .leading, spacing: large ? 18 : 10) {
            HStack(alignment: .center, spacing: 14) {
                Image(systemName: Self.symbol(sky)).symbolRenderingMode(.multicolor).font(.system(size: large ? 46 : 30))
                VStack(alignment: .leading, spacing: 1) {
                    Text(data["place"] as? String ?? "Weather").font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1)
                    Text(num(now["temp_c"])).font(.system(size: large ? 52 : 34, weight: .semibold, design: .rounded)).foregroundStyle(Theme.text)
                }
                Spacer()
                VStack(alignment: .trailing, spacing: 2) {
                    Text(sky.capitalized).font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
                    Text("H \(num(today["max_c"]))  L \(num(today["min_c"]))").font(Theme.caption).foregroundStyle(Theme.text2)
                }
            }
            HStack(spacing: 8) {
                tile("thermometer.medium", "Feels like", num(now["feels_c"]))
                tile("wind", "Wind", String(format: "%.0f km/h", dbl(now["wind_kmh"])))
                tile("drop.fill", "Rain", "\(today["rain_chance_pct"] as? Int ?? 0)%")
            }
            if days.count > 1 {
                VStack(spacing: large ? 8 : 5) {
                    ForEach(Array(days.prefix(large ? 7 : 3).enumerated()), id: \.offset) { _, d in
                        HStack(spacing: 10) {
                            Text(dayName(d["date"] as? String ?? "")).font(Theme.body).foregroundStyle(Theme.text).frame(width: large ? 90 : 64, alignment: .leading)
                            Image(systemName: Self.symbol(d["sky"] as? String ?? "")).symbolRenderingMode(.multicolor).frame(width: 22)
                            Text("\(d["rain_chance_pct"] as? Int ?? 0)%").font(Theme.caption).foregroundStyle(Theme.accent).frame(width: 34, alignment: .trailing)
                            Spacer()
                            Text("\(num(d["min_c"])) – \(num(d["max_c"]))").font(Theme.body.monospacedDigit()).foregroundStyle(Theme.text2)
                        }
                    }
                }
                .padding(.top, 2)
            }
        }
        .padding(large ? 18 : 0)
    }

    private func tile(_ icon: String, _ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Image(systemName: icon).font(.system(size: 11, weight: .semibold)).foregroundStyle(Theme.accent)
            Text(value).font(.system(size: large ? 18 : 15, weight: .semibold, design: .rounded)).foregroundStyle(Theme.text)
            Text(label).font(Theme.micro).foregroundStyle(Theme.text3)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
    }

    private func dayName(_ iso: String) -> String {
        let f = DateFormatter(); f.dateFormat = "yyyy-MM-dd"
        guard let d = f.date(from: iso) else { return iso }
        if Calendar.current.isDateInToday(d) { return "Today" }
        if Calendar.current.isDateInTomorrow(d) { return "Tomorrow" }
        let o = DateFormatter(); o.dateFormat = "EEE d"
        return o.string(from: d)
    }
}

// MARK: - Reminder / event added

struct AddedView: View {
    let view: [String: Any]
    var large = false

    private var isEvent: Bool { view["type"] as? String == "event" }
    private var date: Date? {
        let s = (isEvent ? view["start"] : view["due"]) as? String ?? ""
        let f = ISO8601DateFormatter()
        return f.date(from: s) ?? { f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]; return f.date(from: s) }()
    }

    var body: some View {
        let d = date
        let size: CGFloat = large ? 72 : 46
        HStack(spacing: large ? 18 : 12) {
            VStack(spacing: 0) {
                Text(d.map { $0.formatted(.dateTime.month(.abbreviated)).uppercased() } ?? (isEvent ? "EVENT" : "TO DO"))
                    .font(.system(size: size * 0.2, weight: .bold)).foregroundStyle(.white)
                    .frame(maxWidth: .infinity).frame(height: size * 0.3).background(Theme.bad)
                Text(d.map { $0.formatted(.dateTime.day()) } ?? "✓")
                    .font(.system(size: size * 0.42, weight: .semibold, design: .rounded)).foregroundStyle(.black)
                    .frame(maxWidth: .infinity, maxHeight: .infinity).background(.white)
            }
            .frame(width: size, height: size)
            .clipShape(RoundedRectangle(cornerRadius: size * 0.22, style: .continuous))
            VStack(alignment: .leading, spacing: 3) {
                Text(isEvent ? "ADDED TO CALENDAR" : "REMINDER ADDED").font(Theme.overline).tracking(0.8).foregroundStyle(Theme.good)
                Text(view["title"] as? String ?? "").font(large ? Theme.h2 : Theme.title).foregroundStyle(Theme.text).lineLimit(2)
                Text(when(d)).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1)
            }
            Spacer(minLength: 0)
            Image(systemName: "checkmark.circle.fill").font(.system(size: large ? 26 : 20)).foregroundStyle(Theme.good)
        }
        .padding(large ? 18 : 0)
    }

    private func when(_ d: Date?) -> String {
        var parts: [String] = []
        if let d {
            var t = d.formatted(.dateTime.weekday(.wide).hour().minute())
            if isEvent, let e = view["end"] as? String, let ed = ISO8601DateFormatter().date(from: e) { t += " – " + ed.formatted(.dateTime.hour().minute()) }
            parts.append(t)
        }
        for k in ["location", "calendar", "list"] { if let v = view[k] as? String, !v.isEmpty { parts.append(v) } }
        return parts.isEmpty ? "No date" : parts.joined(separator: " · ")
    }
}

// MARK: - Agenda (calendar look-ups)

struct AgendaView: View {
    let view: [String: Any]
    var large = false
    /// "Thu 2026-10-15 09:30" (the server's local format) → (day key, time).
    private func split(_ s: String) -> (String, String) {
        let p = s.split(separator: " ")
        return p.count >= 3 ? ("\(p[0]) \(p[1])", String(p[2])) : (s, "")
    }
    private func dayTitle(_ key: String) -> String {
        let f = DateFormatter(); f.dateFormat = "EEE yyyy-MM-dd"; f.locale = Locale(identifier: "en_US_POSIX")
        guard let d = f.date(from: key) else { return key }
        if Calendar.current.isDateInToday(d) { return "Today" }
        if Calendar.current.isDateInTomorrow(d) { return "Tomorrow" }
        return d.formatted(.dateTime.weekday(.wide).day().month(.wide))
    }
    var body: some View {
        let events = (view["events"] as? [[String: Any]]) ?? []
        let groups = Dictionary(grouping: events) { split($0["start"] as? String ?? "").0 }
        let keys = groups.keys.sorted { a, b in (events.firstIndex { split($0["start"] as? String ?? "").0 == a } ?? 0) < (events.firstIndex { split($0["start"] as? String ?? "").0 == b } ?? 0) }
        VStack(alignment: .leading, spacing: large ? 14 : 8) {
            HStack(spacing: 10) {
                IconTile(symbol: "calendar", size: large ? 34 : 26, tone: Theme.bad)
                Text(events.isEmpty ? "Nothing in your calendar" : "\(events.count) event\(events.count == 1 ? "" : "s")").font(large ? Theme.h3 : Theme.title).foregroundStyle(Theme.text)
                Spacer()
            }
            ForEach(Array(keys.prefix(large ? 14 : 2)), id: \.self) { k in
                VStack(alignment: .leading, spacing: 5) {
                    Text(dayTitle(k).uppercased()).font(Theme.overline).tracking(0.8).foregroundStyle(Theme.text3)
                    ForEach(Array((groups[k] ?? []).prefix(large ? 20 : 3).enumerated()), id: \.offset) { _, e in
                        HStack(spacing: 10) {
                            RoundedRectangle(cornerRadius: 2).fill(Theme.accent).frame(width: 3, height: large ? 34 : 26)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(e["title"] as? String ?? "").font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
                                Text([split(e["start"] as? String ?? "").1 + (split(e["end"] as? String ?? "").1.isEmpty ? "" : " – " + split(e["end"] as? String ?? "").1), e["location"] as? String ?? "", e["calendar"] as? String ?? ""].filter { !$0.isEmpty }.joined(separator: " · "))
                                    .font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1)
                            }
                        }
                    }
                }
            }
        }
        .padding(large ? 18 : 0)
    }
}

// MARK: - Open reminders

struct TodosView: View {
    let items: [[String: Any]]
    var large = false
    var body: some View {
        VStack(alignment: .leading, spacing: large ? 10 : 6) {
            HStack(spacing: 10) {
                IconTile(symbol: "checklist", size: large ? 34 : 26, tone: .orange)
                Text(items.isEmpty ? "Nothing open" : "\(items.count) open").font(large ? Theme.h3 : Theme.title).foregroundStyle(Theme.text)
                Spacer()
            }
            ForEach(Array(items.prefix(large ? 40 : 4).enumerated()), id: \.offset) { _, r in
                HStack(spacing: 10) {
                    Circle().strokeBorder(Theme.text3, lineWidth: 1.5).frame(width: 16, height: 16)
                    Text(r["title"] as? String ?? "").font(Theme.body).foregroundStyle(Theme.text).lineLimit(1)
                    Spacer()
                    Text([r["due"] as? String ?? "", large ? (r["list"] as? String ?? "") : ""].filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
            }
            if !large && items.count > 4 { Text("and \(items.count - 4) more").font(Theme.caption).foregroundStyle(Theme.text3) }
        }
        .padding(large ? 18 : 0)
    }
}

// MARK: - Files found

struct FilesView: View {
    let paths: [String]
    var large = false
    var body: some View {
        VStack(alignment: .leading, spacing: large ? 8 : 5) {
            ForEach(Array(paths.prefix(large ? 20 : 3).enumerated()), id: \.offset) { _, p in
                Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: p)]) } label: {
                    HStack(spacing: 10) {
                        Image(nsImage: NSWorkspace.shared.icon(forFile: p)).resizable().frame(width: large ? 34 : 26, height: large ? 34 : 26)
                        VStack(alignment: .leading, spacing: 1) {
                            Text((p as NSString).lastPathComponent).font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text).lineLimit(1)
                            Text((p as NSString).deletingLastPathComponent.replacingOccurrences(of: NSHomeDirectory(), with: "~")).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.middle)
                        }
                        Spacer()
                        Image(systemName: "magnifyingglass").font(.system(size: 10, weight: .semibold)).foregroundStyle(Theme.text3)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Show in Finder")
            }
            if !large && paths.count > 3 { Text("and \(paths.count - 3) more").font(Theme.caption).foregroundStyle(Theme.text3) }
        }
        .padding(large ? 18 : 0)
    }
}
