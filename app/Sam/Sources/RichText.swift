// Rich rendering for task results in the overview: markdown (headings, bullets, tables, code,
// inline bold/italic/links), JSON as key–value rows or tables, clickable URLs and file paths,
// and image thumbnails (web URLs, or a task's own files fetched via the server's /api/file,
// since the app can't read ~/Documents without a privacy prompt).
import AppKit
import SwiftUI
import UniformTypeIdentifiers

enum Remote {
    static var base = URL(string: "http://127.0.0.1:47800/")!

    /// A task file's bytes through the server (token-protected, artifacts / task files only).
    static func file(_ path: String) async -> Data? {
        var c = URLComponents(url: base.appendingPathComponent("api/file"), resolvingAgainstBaseURL: false)!
        c.queryItems = [URLQueryItem(name: "path", value: path)]
        var req = URLRequest(url: c.url!, timeoutInterval: 10)
        LocalAuth.sign(&req)
        guard let (d, r) = try? await URLSession.shared.data(for: req), (r as? HTTPURLResponse)?.statusCode == 200 else { return nil }
        return d
    }
}

enum RichKind {
    static let imageExts: Set<String> = ["png", "jpg", "jpeg", "gif", "webp", "heic"]
    static let textExts: Set<String> = ["md", "txt", "csv", "json", "jsonl", "log"]
    static func ext(_ s: String) -> String { (s.split(separator: "?").first.map(String.init) ?? s).components(separatedBy: ".").last?.lowercased() ?? "" }
    static func isImage(_ s: String) -> Bool { imageExts.contains(ext(s)) }
}

/// Markdown / JSON / plain text, rendered as blocks.
struct RichText: View {
    let text: String

    private enum Block: Identifiable {
        case heading(Int, String, Int), para(String, Int), bullets([String], Int), table([[String]], Int), code(String, Int), json(Any, Int), image(String, Int)
        var id: Int {
            switch self {
            case .heading(_, _, let i), .para(_, let i), .bullets(_, let i), .table(_, let i), .code(_, let i), .json(_, let i), .image(_, let i): return i
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(blocks) { b in view(b) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder private func view(_ b: Block) -> some View {
        switch b {
        case .heading(let level, let t, _):
            Text(inline(t)).font(.system(size: level <= 1 ? 17 : (level == 2 ? 15 : 13), weight: .semibold)).foregroundStyle(Theme.text).padding(.top, 2)
        case .para(let t, _):
            Text(inline(t)).font(Theme.body).foregroundStyle(Theme.text2).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
        case .bullets(let items, _):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, it in
                    HStack(alignment: .firstTextBaseline, spacing: 7) {
                        Text("•").foregroundStyle(Theme.text3)
                        Text(inline(it)).font(Theme.body).foregroundStyle(Theme.text2).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        case .table(let rows, _):
            TableView(rows: rows)
        case .code(let t, _):
            ScrollView(.horizontal) { Text(t).font(Theme.mono).foregroundStyle(Theme.text2).textSelection(.enabled).padding(10) }
                .background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
        case .json(let v, _):
            JSONView(value: v, depth: 0)
        case .image(let src, _):
            Thumb(src: src)
        }
    }

    private var blocks: [Block] {
        var out: [Block] = []
        var n = 0
        func add(_ b: (Int) -> Block) { out.append(b(n)); n += 1 }
        // "tool: {json…}" → the prefix as text, the JSON as a structured view.
        if let i = text.firstIndex(where: { $0 == "{" || $0 == "[" }), let d = String(text[i...]).data(using: .utf8),
           let v = try? JSONSerialization.jsonObject(with: d), v is [String: Any] || v is [Any] {
            let pre = text[..<i].trimmingCharacters(in: .whitespacesAndNewlines).trimmingCharacters(in: CharacterSet(charactersIn: ":"))
            if !pre.isEmpty { add { .para(pre, $0) } }
            add { .json(v, $0) }
            return out
        }
        let lines = text.components(separatedBy: "\n")
        var i = 0, para: [String] = [], bullets: [String] = []
        func flush() {
            if !para.isEmpty { let p = para.joined(separator: " "); add { .para(p, $0) }; para = [] }
            if !bullets.isEmpty { let b = bullets; add { .bullets(b, $0) }; bullets = [] }
        }
        while i < lines.count {
            let l = lines[i].trimmingCharacters(in: .whitespaces)
            if l.hasPrefix("```") {
                flush()
                var code: [String] = []; i += 1
                while i < lines.count, !lines[i].trimmingCharacters(in: .whitespaces).hasPrefix("```") { code.append(lines[i]); i += 1 }
                let c = code.joined(separator: "\n"); add { .code(c, $0) }
            } else if l.hasPrefix("|"), i + 1 < lines.count, lines[i + 1].contains("---") {
                flush()
                var rows: [[String]] = [cells(l)]; i += 2
                while i < lines.count, lines[i].trimmingCharacters(in: .whitespaces).hasPrefix("|") { rows.append(cells(lines[i])); i += 1 }
                i -= 1
                let r = rows; add { .table(r, $0) }
            } else if let h = l.firstIndex(where: { $0 != "#" }), l.hasPrefix("#"), l[h] == " " {
                flush()
                let level = l.distance(from: l.startIndex, to: h), t = String(l[h...]).trimmingCharacters(in: .whitespaces)
                add { .heading(level, t, $0) }
            } else if l.hasPrefix("- ") || l.hasPrefix("* ") || l.range(of: #"^\d+[.)] "#, options: .regularExpression) != nil {
                if !para.isEmpty { let p = para.joined(separator: " "); add { .para(p, $0) }; para = [] }
                bullets.append(l.replacingOccurrences(of: #"^([-*]|\d+[.)]) "#, with: "", options: .regularExpression))
            } else if let img = imageSource(l) {
                flush(); add { .image(img, $0) }
            } else if l.isEmpty || l == "---" {
                flush()
            } else {
                if !bullets.isEmpty { let b = bullets; add { .bullets(b, $0) }; bullets = [] }
                para.append(l)
            }
            i += 1
        }
        flush()
        return out
    }

    private func cells(_ l: String) -> [String] {
        l.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "|")).components(separatedBy: "|").map { $0.trimmingCharacters(in: .whitespaces) }
    }

    /// A line that is just an image: ![alt](src), or a bare image URL / path.
    private func imageSource(_ l: String) -> String? {
        if let m = l.range(of: #"^!\[[^\]]*\]\(([^)]+)\)$"#, options: .regularExpression) {
            let s = String(l[m]); return String(s[s.index(after: s.firstIndex(of: "(")!)..<s.index(before: s.endIndex)])
        }
        let bare = l.trimmingCharacters(in: CharacterSet(charactersIn: "<>"))
        if !bare.contains(" "), (bare.hasPrefix("http") || bare.hasPrefix("/")), RichKind.isImage(bare) { return bare }
        return nil
    }

    /// Inline markdown with bare URLs and absolute file paths turned into links.
    private func inline(_ s: String) -> AttributedString {
        var t = s
        // Bare URLs (not already inside a markdown link) → [url](url).
        t = t.replacingOccurrences(of: #"(?<![(\[<])(https?://[^\s)\]>]+)"#, with: "[$1]($1)", options: .regularExpression)
        // Absolute file paths with an extension → file links (open in the default app).
        t = t.replacingOccurrences(of: #"(?<![(\[/\w])(/(?:Users|tmp|private|Volumes)/[^\s)\]]+\.[A-Za-z0-9]{1,5})"#, with: "[$1](file://$1)", options: .regularExpression)
        let opt = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        var a = (try? AttributedString(markdown: t, options: opt)) ?? AttributedString(s)
        for run in a.runs where run.link != nil { a[run.range].foregroundColor = Theme.accent }
        return a
    }
}

struct TableView: View {
    let rows: [[String]]
    var body: some View {
        let cols = rows.map(\.count).max() ?? 0
        ScrollView(.horizontal) {
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 6) {
                ForEach(Array(rows.prefix(30).enumerated()), id: \.offset) { r, row in
                    GridRow {
                        ForEach(0..<cols, id: \.self) { c in
                            Text(c < row.count ? row[c] : "").font(r == 0 ? Theme.caption.weight(.semibold) : Theme.body).foregroundStyle(r == 0 ? Theme.text3 : Theme.text2).lineLimit(2).textSelection(.enabled)
                        }
                    }
                    if r == 0 { Divider().gridCellUnsizedAxes(.horizontal) }
                }
            }
            .padding(10)
        }
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
    }
}

/// JSON as readable rows: objects → key/value, arrays of objects → a table, scalars inline.
struct JSONView: View {
    let value: Any
    let depth: Int

    var body: some View {
        if let o = value as? [String: Any] {
            VStack(alignment: .leading, spacing: 5) {
                ForEach(o.keys.sorted(), id: \.self) { k in
                    if let v = o[k], isScalar(v) {
                        HStack(alignment: .firstTextBaseline, spacing: 10) {
                            Text(label(k)).font(Theme.caption).foregroundStyle(Theme.text3).frame(minWidth: 90, alignment: .leading)
                            Text(scalar(v)).font(Theme.body).foregroundStyle(Theme.text2).textSelection(.enabled)
                        }
                    } else if let v = o[k] {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(label(k)).font(Theme.caption.weight(.semibold)).foregroundStyle(Theme.text3)
                            JSONView(value: v, depth: depth + 1).padding(.leading, 10)
                        }
                    }
                }
            }
        } else if let a = value as? [Any] {
            if let objs = a as? [[String: Any]], !objs.isEmpty, objs.allSatisfy({ $0.values.allSatisfy(isScalar) }) {
                let keys = Array(Set(objs.flatMap(\.keys))).sorted()
                TableView(rows: [keys.map(label)] + objs.prefix(30).map { o in keys.map { o[$0].map(scalar) ?? "" } })
            } else if a.allSatisfy(isScalar) {
                Text(a.map(scalar).joined(separator: ", ")).font(Theme.body).foregroundStyle(Theme.text2).textSelection(.enabled)
            } else {
                VStack(alignment: .leading, spacing: 6) { ForEach(Array(a.prefix(20).enumerated()), id: \.offset) { _, v in JSONView(value: v, depth: depth + 1) } }
            }
        } else {
            Text(scalar(value)).font(Theme.body).foregroundStyle(Theme.text2)
        }
    }

    private func isScalar(_ v: Any) -> Bool { !(v is [String: Any]) && !(v is [Any]) }
    private func label(_ k: String) -> String { k.replacingOccurrences(of: "_", with: " ").capitalized }
    private func scalar(_ v: Any) -> String {
        switch v {
        case let s as String: return s
        case let b as Bool: return b ? "yes" : "no"
        case let n as NSNumber: return n.stringValue
        case is NSNull: return "—"
        default: return "\(v)"
        }
    }
}

/// An image from the web or from a task's own files (via the server).
struct Thumb: View {
    let src: String
    @State private var local: NSImage?
    var body: some View {
        Group {
            if src.hasPrefix("http"), let u = URL(string: src) {
                AsyncImage(url: u) { img in img.resizable().scaledToFit() } placeholder: { ProgressView().controlSize(.small) }
            } else if let local {
                Image(nsImage: local).resizable().scaledToFit()
            } else {
                ProgressView().controlSize(.small).task { if let d = await Remote.file(src) { local = NSImage(data: d) } }
            }
        }
        .frame(maxWidth: 420, maxHeight: 280, alignment: .leading)
        .clipShape(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous))
        .onTapGesture { if src.hasPrefix("http"), let u = URL(string: src) { NSWorkspace.shared.open(u) } else { NSWorkspace.shared.open(URL(fileURLWithPath: src)) } }
    }
}

/// First lines without markdown syntax (collapsed timeline rows).
func plainPreview(_ s: String) -> String {
    // The first paragraph only: stop at a table, code block or blank line (tables read badly flat).
    var lines: [String] = []
    for l in s.components(separatedBy: "\n") {
        let t = l.trimmingCharacters(in: .whitespaces)
        if t.hasPrefix("|") || t.hasPrefix("```") || (t.isEmpty && !lines.isEmpty) { break }
        if !t.isEmpty { lines.append(t) }
    }
    var t = lines.joined(separator: " ").replacingOccurrences(of: #"[#*_`>|]+"#, with: " ", options: .regularExpression)
    t = t.replacingOccurrences(of: #"\[([^\]]+)\]\([^)]+\)"#, with: "$1", options: .regularExpression)
    if let i = t.firstIndex(where: { $0 == "{" }), t.distance(from: t.startIndex, to: i) < 40 { t = String(t[..<i]) + "…" }
    return t.replacingOccurrences(of: #"\s+"#, with: " ", options: .regularExpression).trimmingCharacters(in: .whitespaces)
}
