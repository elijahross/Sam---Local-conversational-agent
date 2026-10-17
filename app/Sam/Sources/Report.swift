// A task's presented result (server/src/present.rs) in the overview, Perplexity-style: hero
// image, cited summary, key facts, the forecast when it's close, a map, highlight cards with
// pictures, sections, budget and numbered sources. The steps and files stay below it.
import AppKit
import MapKit
import SwiftUI

struct ReportView: View {
    let r: [String: Any]
    var onOpen: (String) -> Void

    private func s(_ k: String) -> String { r[k] as? String ?? "" }
    private var sources: [[String: Any]] { r["sources"] as? [[String: Any]] ?? [] }

    /// "[2]" → a small link to source 2.
    private func cited(_ text: String) -> String {
        var out = text
        for src in sources {
            guard let n = src["n"] as? Int, let url = src["url"] as? String else { continue }
            out = out.replacingOccurrences(of: "[\(n)]", with: "[⁽\(n)⁾](\(url))")
        }
        return out
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            hero
            if !s("summary").isEmpty {
                RichText(text: cited(s("summary"))).font(Theme.lead).lineSpacing(3)
            }
            if let facts = r["facts"] as? [[String: Any]], !facts.isEmpty { factsGrid(facts) }
            if let w = r["weather"] as? [String: Any], let days = w["daily"] as? [[String: Any]], !days.isEmpty { forecast(days) }
            if let p = r["place"] as? [String: Any], let lat = p["lat"] as? Double, let lon = p["lon"] as? Double { map(lat, lon, p["label"] as? String ?? "") }
            if let hs = r["highlights"] as? [[String: Any]], !hs.isEmpty { highlights(hs) }
            if let secs = r["sections"] as? [[String: Any]] {
                ForEach(Array(secs.enumerated()), id: \.offset) { _, sec in
                    VStack(alignment: .leading, spacing: 8) {
                        Text(sec["heading"] as? String ?? "").font(Theme.h3).foregroundStyle(Theme.text)
                        RichText(text: cited(sec["text"] as? String ?? ""))
                    }
                }
            }
            if let b = r["budget"] as? [[String: Any]], !b.isEmpty { budget(b) }
            if !sources.isEmpty { sourceList }
        }
    }

    // MARK: pieces

    @ViewBuilder private var hero: some View {
        let title = Text(s("title")).font(Theme.h1).foregroundStyle(.white)
        let sub = Text(s("subtitle")).font(Theme.body.weight(.medium)).foregroundStyle(.white.opacity(0.85))
        if let h = r["hero"] as? [String: Any], let img = h["image"] as? String {
            ZStack(alignment: .bottomLeading) {
                ReportImage(src: img).frame(height: 220).frame(maxWidth: .infinity).clipped()
                LinearGradient(colors: [.clear, .black.opacity(0.75)], startPoint: .center, endPoint: .bottom)
                VStack(alignment: .leading, spacing: 4) { title; sub }.padding(18)
                Text(h["credit"] as? String ?? "").font(.system(size: 9)).foregroundStyle(.white.opacity(0.6)).padding(8)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
            }
            .frame(height: 220)
            .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
        } else {
            VStack(alignment: .leading, spacing: 4) {
                title.foregroundStyle(Theme.text)
                sub.foregroundStyle(Theme.text2)
            }
        }
    }

    private func factsGrid(_ facts: [[String: Any]]) -> some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 150), spacing: 10)], alignment: .leading, spacing: 10) {
            ForEach(Array(facts.enumerated()), id: \.offset) { _, f in
                VStack(alignment: .leading, spacing: 6) {
                    Image(systemName: (f["icon"] as? String).flatMap { NSImage(systemSymbolName: $0, accessibilityDescription: nil) != nil ? $0 : nil } ?? "info.circle")
                        .font(Theme.title).foregroundStyle(Theme.accent)
                    Text(f["label"] as? String ?? "").font(Theme.caption).foregroundStyle(Theme.text3)
                    Text(f["value"] as? String ?? "").font(Theme.title).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true)
                }
                .padding(12).frame(maxWidth: .infinity, minHeight: 92, alignment: .topLeading)
                .background(RoundedRectangle(cornerRadius: Theme.cardRadius, style: .continuous).fill(Theme.fill))
                .overlay(RoundedRectangle(cornerRadius: Theme.cardRadius, style: .continuous).stroke(Theme.stroke))
            }
        }
    }

    private func forecast(_ days: [[String: Any]]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Forecast", systemImage: "cloud.sun").font(Theme.title).foregroundStyle(Theme.text)
            ScrollView(.horizontal) {
                HStack(spacing: 8) {
                    ForEach(Array(days.enumerated()), id: \.offset) { _, d in
                        VStack(spacing: 4) {
                            Text(String((d["date"] as? String ?? "").suffix(5))).font(Theme.caption).foregroundStyle(Theme.text3)
                            Image(systemName: sky(d["sky"] as? String ?? "")).font(.system(size: 18)).foregroundStyle(Theme.accent).frame(height: 22)
                            Text("\(num(d["min_c"]))–\(num(d["max_c"]))°").font(Theme.label).foregroundStyle(Theme.text)
                            Text("\(num(d["rain_chance_pct"]))%").font(Theme.caption).foregroundStyle(Theme.text3)
                        }
                        .padding(10).frame(width: 74)
                        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
                    }
                }
            }
            .scrollIndicators(.never)
        }
    }

    private func map(_ lat: Double, _ lon: Double, _ label: String) -> some View {
        let pos = MapCameraPosition.region(MKCoordinateRegion(center: CLLocationCoordinate2D(latitude: lat, longitude: lon), span: MKCoordinateSpan(latitudeDelta: 0.6, longitudeDelta: 0.6)))
        return Map(initialPosition: pos, interactionModes: [.pan, .zoom]) {
            Marker(label, coordinate: CLLocationCoordinate2D(latitude: lat, longitude: lon))
        }
        .frame(height: 200)
        .clipShape(RoundedRectangle(cornerRadius: Theme.cardRadius, style: .continuous))
        .overlay(alignment: .bottomTrailing) {
            Button {
                if let u = URL(string: "maps://?ll=\(lat),\(lon)&q=\(label.addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? "")") { NSWorkspace.shared.open(u) }
            } label: { Label("Open in Maps", systemImage: "map") }
                .samButton().controlSize(.small).padding(10)
        }
    }

    private func highlights(_ hs: [[String: Any]]) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Highlights").font(Theme.h3).foregroundStyle(Theme.text)
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 220), spacing: 12)], alignment: .leading, spacing: 12) {
                ForEach(Array(hs.enumerated()), id: \.offset) { _, h in
                    VStack(alignment: .leading, spacing: 0) {
                        if let img = h["image"] as? String {
                            ReportImage(src: img).frame(height: 130).frame(maxWidth: .infinity).clipped()
                        } else {
                            ZStack { Theme.fill; Image(systemName: "photo").foregroundStyle(Theme.text3) }.frame(height: 60)
                        }
                        VStack(alignment: .leading, spacing: 6) {
                            HStack(alignment: .firstTextBaseline) {
                                Text(h["title"] as? String ?? "").font(Theme.title).foregroundStyle(Theme.text).lineLimit(2)
                                Spacer(minLength: 4)
                                if let tag = h["tag"] as? String, !tag.isEmpty {
                                    Pill(text: tag, color: Theme.accent)
                                }
                            }
                            RichText(text: cited(h["text"] as? String ?? ""))
                        }
                        .padding(12)
                    }
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                    .samCard()
                }
            }
        }
    }

    private func budget(_ rows: [[String: Any]]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Budget", systemImage: "eurosign.circle").font(Theme.title).foregroundStyle(Theme.text)
            VStack(spacing: 0) {
                ForEach(Array(rows.enumerated()), id: \.offset) { i, b in
                    HStack {
                        Text(b["item"] as? String ?? "").foregroundStyle(Theme.text2)
                        Spacer()
                        Text(b["cost"] as? String ?? "").font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text)
                    }
                    .font(Theme.body).padding(.vertical, 8).padding(.horizontal, 12)
                    .background(i % 2 == 0 ? Theme.fill : .clear)
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous))
        }
    }

    private var sourceList: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Sources").font(Theme.title).foregroundStyle(Theme.text)
            ForEach(Array(sources.enumerated()), id: \.offset) { _, src in
                Button {
                    if let u = (src["url"] as? String).flatMap(URL.init(string:)) { NSWorkspace.shared.open(u) }
                } label: {
                    HStack(spacing: 10) {
                        Text("\(src["n"] as? Int ?? 0)").font(.system(size: 11, weight: .bold, design: .rounded)).frame(width: 22, height: 22)
                            .background(Circle().fill(Theme.accent.opacity(0.2))).foregroundStyle(Theme.accent)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(src["title"] as? String ?? "").font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(1)
                            Text(src["domain"] as? String ?? "").font(Theme.caption).foregroundStyle(Theme.text3)
                        }
                        Spacer()
                        Image(systemName: "arrow.up.right").font(Theme.overline).foregroundStyle(Theme.text3)
                    }
                    .padding(8).background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func num(_ v: Any?) -> String {
        if let d = v as? Double { return String(Int(d.rounded())) }
        if let i = v as? Int { return String(i) }
        return "–"
    }
    private func sky(_ s: String) -> String {
        switch s {
        case "clear": return "sun.max"
        case "partly cloudy": return "cloud.sun"
        case "overcast": return "cloud"
        case "fog": return "cloud.fog"
        case "drizzle", "rain", "rain showers": return "cloud.rain"
        case "snow", "snow showers": return "cloud.snow"
        case "thunderstorm": return "cloud.bolt.rain"
        default: return "cloud"
        }
    }
}

/// A report picture from the task's folder (through the server), filling its frame.
struct ReportImage: View {
    let src: String
    @State private var img: NSImage?
    var body: some View {
        GeometryReader { g in
            Group {
                if let img {
                    Image(nsImage: img).resizable().scaledToFill().frame(width: g.size.width, height: g.size.height).clipped()
                } else {
                    ZStack { Theme.fill; ProgressView().controlSize(.small) }
                }
            }
        }
        .task(id: src) { if img == nil, let d = await Remote.file(src) { img = NSImage(data: d) } }
    }
}
