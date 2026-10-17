// Connectors: add and manage the integrations Sam can use (GET/POST /api/connectors).
// Built-in Mac toolkits, email, chat and web sessions, website logins, file folders and MCP
// integrations (catalog templates or custom). Secrets typed here go to the server, which keeps
// them in the macOS Keychain only.
import AppKit
import SwiftUI

struct Connector: Identifiable, Equatable {
    let id, name, icon, category, description, status, detail, kind: String
    let tools: [String]
    let actions: [String]
    let items: [(label: String, remove: String)]
    let canDisable: Bool
    /// Chat services: can Sam drive the installed app, and which way is chosen ("app" | "web").
    let appAvailable: Bool
    let via: String
    let appNote: String
    static func == (a: Connector, b: Connector) -> Bool { a.id == b.id && a.status == b.status && a.detail == b.detail && a.items.map(\.label) == b.items.map(\.label) && a.via == b.via }
    init?(_ j: [String: Any]) {
        guard let id = j["id"] as? String else { return nil }
        let s = { (k: String) in j[k] as? String ?? "" }
        self.id = id; name = s("name"); icon = s("icon"); category = s("category"); description = s("description"); status = s("status"); detail = s("detail"); kind = s("kind")
        tools = j["tools"] as? [String] ?? []
        actions = j["actions"] as? [String] ?? []
        items = ((j["items"] as? [[String: Any]]) ?? []).map { ($0["label"] as? String ?? "", $0["remove"] as? String ?? "") }
        canDisable = j["can_disable"] as? Bool ?? false
        let o = j["app_option"] as? [String: Any]
        appAvailable = o?["available"] as? Bool ?? false
        via = o?["via"] as? String ?? ""
        appNote = o?["note"] as? String ?? ""
    }
}

struct CatalogItem: Identifiable {
    let id, name, icon, description, command: String
    let env: [String]
}

@MainActor
final class ConnectorsStore: ObservableObject {
    @Published private(set) var connectors: [Connector] = []
    @Published private(set) var catalog: [CatalogItem] = []
    @Published var message: (text: String, ok: Bool)?
    @Published var busy = false
    let base: URL
    init(base: URL) { self.base = base }

    func refresh() async {
        var req = URLRequest(url: base.appendingPathComponent("api/connectors"), timeoutInterval: 5)
        LocalAuth.sign(&req)
        req.cachePolicy = .reloadIgnoringLocalCacheData
        guard let (d, _) = try? await URLSession.shared.data(for: req), let j = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any] else {
            message = ("Sam's server isn't running.", false); return
        }
        connectors = ((j["connectors"] as? [[String: Any]]) ?? []).compactMap(Connector.init)
        catalog = ((j["catalog"] as? [[String: Any]]) ?? []).map {
            CatalogItem(id: $0["id"] as? String ?? "", name: $0["name"] as? String ?? "", icon: $0["icon"] as? String ?? "puzzlepiece.extension",
                        description: $0["description"] as? String ?? "", command: (($0["command"] as? String ?? "") + " " + (($0["args"] as? [String]) ?? []).joined(separator: " ")), env: $0["env"] as? [String] ?? [])
        }
    }

    func act(_ id: String, _ action: String, _ values: [String: Any] = [:]) {
        busy = true
        var req = URLRequest(url: base.appendingPathComponent("api/connectors/\(id)"), timeoutInterval: 90)
        LocalAuth.sign(&req)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("1", forHTTPHeaderField: "X-Sam")
        req.httpBody = try? JSONSerialization.data(withJSONObject: ["action": action, "values": values])
        Task {
            let r = try? await URLSession.shared.data(for: req)
            let j = r.flatMap { (try? JSONSerialization.jsonObject(with: $0.0)) as? [String: Any] }
            message = (j?["message"] as? String ?? "No answer from Sam's server.", j?["ok"] as? Bool ?? false)
            busy = false
            await refresh()
        }
    }
}

@MainActor
final class ConnectorsController {
    let store: ConnectorsStore
    private var window: NSWindow?
    var isOpen: Bool { window?.isVisible ?? false }
    init(store: ConnectorsStore) { self.store = store }

    func show(screen: NSScreen? = nil) {
        if window == nil {
            let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 940, height: 640), styleMask: [.titled, .closable, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
            w.title = "Sam Connectors"
            w.titlebarAppearsTransparent = true
            w.titleVisibility = .hidden
            w.isMovableByWindowBackground = true
            w.isReleasedWhenClosed = false
            w.isRestorable = false   // macOS window restoration would reopen it at relaunch
            w.minSize = NSSize(width: 760, height: 520)
            w.appearance = NSAppearance(named: .darkAqua)
            w.backgroundColor = NSColor(Theme.bgDeep)
            let blur = NSVisualEffectView()
            blur.material = .underWindowBackground; blur.blendingMode = .behindWindow; blur.state = .active
            let host = NSHostingView(rootView: ZStack { AppBackdrop(); ConnectorsView(store: store) }.tint(Theme.accent))
            host.translatesAutoresizingMaskIntoConstraints = false
            blur.addSubview(host)
            NSLayoutConstraint.activate([host.leadingAnchor.constraint(equalTo: blur.leadingAnchor), host.trailingAnchor.constraint(equalTo: blur.trailingAnchor),
                                         host.topAnchor.constraint(equalTo: blur.topAnchor), host.bottomAnchor.constraint(equalTo: blur.bottomAnchor)])
            w.contentView = blur
            w.center()
            window = w
        }
        if let s = screen, let w = window { w.setFrameOrigin(NSPoint(x: s.frame.midX - w.frame.width / 2, y: s.frame.midY - w.frame.height / 2)) }
        Task { await store.refresh() }
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    func hide() { window?.orderOut(nil) }
}

struct ConnectorsView: View {
    @ObservedObject var store: ConnectorsStore
    @State private var category = "All"
    @State private var query = ""
    @State private var selected: Connector?
    @State private var adding = false
    private let categories = ["All", "This Mac", "Email & messaging", "Web", "Files", "Integrations (MCP)"]
    private let icons = ["All": "square.grid.2x2", "This Mac": "laptopcomputer", "Email & messaging": "bubble.left.and.bubble.right", "Web": "globe", "Files": "folder", "Integrations (MCP)": "puzzlepiece.extension"]

    private var shown: [Connector] {
        store.connectors.filter { (category == "All" || $0.category == category) && (query.isEmpty || ($0.name + " " + $0.description).localizedCaseInsensitiveContains(query)) }
    }

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Connectors").font(Theme.h2).padding(.bottom, 10).padding(.top, 34)
                ForEach(categories, id: \.self) { c in
                    Button { category = c } label: {
                        HStack(spacing: 8) {
                            Image(systemName: icons[c] ?? "circle").frame(width: 18)
                            Text(c).lineLimit(1)
                            Spacer()
                            let n = c == "All" ? store.connectors.count : store.connectors.filter { $0.category == c }.count
                            Text("\(n)").font(Theme.caption).foregroundStyle(Theme.text3)
                        }
                        .font(Theme.body).foregroundStyle(category == c ? Theme.text : Theme.text2)
                        .padding(.horizontal, 10).padding(.vertical, 7)
                        .background(RoundedRectangle(cornerRadius: Theme.buttonRadius, style: .continuous).fill((category == c ? Theme.fillSelected : Color.clear)))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                Spacer()
                Button { adding = true } label: { Label("Add integration", systemImage: "plus") .frame(maxWidth: .infinity) }
                    .samButton(prominent: true).controlSize(.large)
            }
            .padding(16)
            .frame(width: 236)
            .background(Theme.glassTint)

            VStack(alignment: .leading, spacing: 14) {
                HStack {
                    Text(category).font(Theme.h3)
                    Spacer()
                    TextField("Search", text: $query).textFieldStyle(.roundedBorder).frame(width: 220)
                }
                .padding(.top, 30)
                ScrollView {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 300), spacing: 14)], spacing: 14) {
                        ForEach(shown) { c in
                            ConnectorCard(c: c, onOpen: { selected = c }, onToggle: { on in store.act(c.id, on ? "enable" : "disable") })
                        }
                    }
                    .padding(.bottom, 14)
                }
                .scrollIndicators(.never)
                if let m = store.message {
                    HStack(spacing: 8) {
                        Image(systemName: m.ok ? "checkmark.circle.fill" : "exclamationmark.triangle.fill").foregroundStyle(m.ok ? Theme.good : Theme.warn)
                        Text(m.text).font(Theme.body).foregroundStyle(Theme.text2).lineLimit(2)
                        Spacer()
                        Button { store.message = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain).foregroundStyle(Theme.text3)
                    }
                    .padding(10).background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
                }
            }
            .padding(.horizontal, 22).padding(.bottom, 18)
        }
        .foregroundStyle(Theme.text)
        .environment(\.colorScheme, .dark)
        .sheet(item: $selected) { c in
            ConnectorDetail(c: store.connectors.first { $0.id == c.id } ?? c, store: store, onClose: { selected = nil })
        }
        .sheet(isPresented: $adding) { AddIntegration(store: store, onClose: { adding = false }) }
        .task { await store.refresh() }
    }
}

struct StatusPill: View {
    let status: String
    var body: some View {
        let (t, c): (String, Color) = switch status {
        case "connected": ("Connected", Theme.good)
        case "setup": ("Set up", .orange)
        case "off": ("Off", Theme.text3)
        default: ("Available", Theme.accent)
        }
        Pill(text: t, color: c)
    }
}

struct ConnectorCard: View {
    let c: Connector
    var onOpen: () -> Void
    var onToggle: (Bool) -> Void
    @State private var hover = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top, spacing: 12) {
                IconTile(symbol: c.icon, size: 40)
                VStack(alignment: .leading, spacing: 3) {
                    Text(c.name).font(Theme.body.weight(.semibold))
                    Text(c.detail).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer()
                StatusPill(status: c.status)
            }
            Text(c.description).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
            HStack {
                Button(primaryLabel, action: onOpen).samButton(prominent: c.status == "setup").controlSize(.small)
                Spacer()
                if c.canDisable {
                    Toggle("", isOn: Binding(get: { c.status != "off" }, set: { onToggle($0) })).toggleStyle(.switch).controlSize(.mini).labelsHidden()
                        .accessibilityLabel("\(c.name) on")
                }
            }
        }
        .padding(14)
        .frame(minHeight: 150, alignment: .top)
        .samCard()
        .overlay(RoundedRectangle(cornerRadius: Theme.cardRadius, style: .continuous).fill(Color.white.opacity(hover ? 0.03 : 0)).allowsHitTesting(false))
        .onHover { hover = $0 }
    }

    private var primaryLabel: String {
        if c.status == "setup" { return c.actions.contains("link") ? "Link" : "Set up" }
        return c.actions.isEmpty ? "Details" : "Manage"
    }
}

struct ConnectorDetail: View {
    let c: Connector
    @ObservedObject var store: ConnectorsStore
    var onClose: () -> Void
    @State private var f: [String: String] = [:]

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 12) {
                IconTile(symbol: c.icon, size: 44)
                VStack(alignment: .leading, spacing: 2) {
                    Text(c.name).font(Theme.h3)
                    Text(c.detail).font(Theme.caption).foregroundStyle(Theme.text3)
                }
                Spacer()
                StatusPill(status: c.status)
            }
            Text(c.description).font(Theme.body).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            if !c.tools.isEmpty {
                HStack(spacing: 6) {
                    Text("Tools").font(Theme.caption).foregroundStyle(Theme.text3)
                    ForEach(c.tools, id: \.self) { Text(TaskText.tool($0)).font(Theme.caption).padding(.horizontal, 7).padding(.vertical, 2).background(Capsule().fill(Theme.fill)) }
                }
            }
            if !c.items.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(Array(c.items.enumerated()), id: \.offset) { _, it in
                        HStack {
                            Text(it.label).font(Theme.body).lineLimit(1).truncationMode(.middle)
                            Spacer()
                            Button("Remove") { store.act(c.id, it.remove, ["item": it.label]) }.samButton().controlSize(.small)
                        }
                        .padding(8).background(RoundedRectangle(cornerRadius: Theme.buttonRadius).fill(Theme.fill))
                    }
                }
            }
            forms
            Spacer(minLength: 0)
            HStack {
                if c.kind == "mcp" { Button("Remove integration", role: .destructive) { store.act(c.id, "remove"); onClose() }.samButton() }
                if store.busy { ProgressView().controlSize(.small) }
                Spacer()
                Button("Done", action: onClose).samButton(prominent: true).keyboardShortcut(.defaultAction)
            }
        }
        .padding(22)
        .frame(width: 520, height: 520)
        .environment(\.colorScheme, .dark)
    }

    @ViewBuilder private var forms: some View {
        if c.actions.contains("setup") {
            formBox("Connect an email account (IMAP/SMTP)", note: "Use an app password (Gmail, iCloud, Outlook: account security settings). It's kept in the Keychain.") {
                field("Email address", "user"); secure("App password", "password"); field("IMAP server (optional)", "imap_host"); field("SMTP server (optional)", "smtp_host")
                Button("Connect") { store.act(c.id, "setup", f) }.samButton(prominent: true)
            }
        }
        if c.actions.contains("disconnect") {
            Button("Disconnect email account") { store.act(c.id, "disconnect") }.samButton()
        }
        if c.actions.contains("chat_via") {
            formBox("How Sam reaches \(c.name)", note: c.appAvailable
                    ? "Your installed app: Sam works in it in the background — drafts wait in the chat's message box, Send is pressed only after your OK. Sam's browser: its own \(c.name) Web session, linked once with a QR code / sign-in."
                    : c.appNote + " Sam uses its own \(c.name) Web session, linked once.") {
                Picker("", selection: Binding(get: { c.via }, set: { store.act(c.id, "chat_via", ["via": $0]) })) {
                    Text("Installed app").tag("app")
                    Text("Sam's browser").tag("web")
                }
                .pickerStyle(.segmented).labelsHidden().disabled(!c.appAvailable)
                if c.via == "app" && c.id == "whatsapp" {
                    HStack {
                        field("Your number, for \u{201C}message myself\u{201D} (+43 …)", "phone")
                        Button("Save") { store.act(c.id, "my_phone", f) }.samButton()
                    }
                }
            }
        }
        if c.actions.contains("link") {
            formBox(c.status == "connected" ? "Linked" : "Link Sam's own session", note: "Sam opens the site in its own browser. If a QR code or sign-in appears, its window comes forward; scan or sign in once.") {
                Button(c.status == "connected" ? "Check link" : "Link now") { store.act(c.id, "link") }.samButton(prominent: c.status != "connected")
            }
        }
        if c.actions.contains("add_login") {
            formBox("Save a website login", note: "Stored in the Keychain; Sam fills it only into that site's matching fields.") {
                field("Website (example.com)", "domain"); field("Username or email", "username"); secure("Password", "password"); secure("2FA seed (optional)", "totp")
                Button("Save login") { store.act(c.id, "add_login", f) }.samButton(prominent: true)
            }
            formBox("Or log in yourself in Sam's browser", note: nil) {
                field("example.com or https://…", "url")
                Button("Open Sam's browser") { store.act(c.id, "browser_login", f) }.samButton()
            }
        }
        if c.actions.contains("notes_backend") {
            formBox("Where notes and the journal go", note: "Apple Notes: folders \u{201C}Sam\u{201D} and \u{201C}Journal\u{201D}. Obsidian: Inbox/ and Daily/ in your vault. Sam only adds, never deletes.") {
                HStack {
                    Button("Apple Notes") { store.act(c.id, "notes_backend", ["backend": "apple"]) }.samButton(prominent: !c.detail.hasPrefix("Obsidian"))
                    Button("Obsidian vault…") { pickVault() }.samButton(prominent: c.detail.hasPrefix("Obsidian"))
                }
            }
        }
        if c.actions.contains("computer_permissions") {
            formBox("Computer use", note: "Sam opens apps in the background and works through Accessibility, so your focus stays where it is. Each goal needs your OK; quitting, closing, saving over files and delete buttons are blocked.") {
                Toggle("Allow Sam to operate apps", isOn: Binding(get: { c.status != "off" }, set: { store.act(c.id, $0 ? "enable" : "disable") })).toggleStyle(.switch)
                Button("Grant helper permissions…") { store.act(c.id, "computer_permissions") }.samButton()
            }
        }
        if c.actions.contains("add_folder") {
            Button { pickFolder() } label: { Label("Add a folder…", systemImage: "folder.badge.plus") }.samButton(prominent: true)
        }
    }

    private func pickVault() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true; p.canChooseFiles = false; p.allowsMultipleSelection = false
        p.message = "Choose your Obsidian vault folder"
        if p.runModal() == .OK, let u = p.url { store.act(c.id, "notes_backend", ["backend": "obsidian", "vault": u.path]) }
    }

    private func pickFolder() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true; p.canChooseFiles = false; p.allowsMultipleSelection = false
        p.message = "Choose a folder Sam may read"
        if p.runModal() == .OK, let u = p.url { store.act(c.id, "add_folder", ["path": u.path]) }
    }

    private func formBox<C: View>(_ title: String, note: String?, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(Theme.body.weight(.semibold))
            if let note { Text(note).font(Theme.caption).foregroundStyle(Theme.text3) }
            content()
        }
        .padding(12).background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
    }
    private func field(_ label: String, _ key: String) -> some View {
        TextField(label, text: Binding(get: { f[key] ?? "" }, set: { f[key] = $0 })).textFieldStyle(.roundedBorder)
    }
    private func secure(_ label: String, _ key: String) -> some View {
        SecureField(label, text: Binding(get: { f[key] ?? "" }, set: { f[key] = $0 })).textFieldStyle(.roundedBorder)
    }
}

struct AddIntegration: View {
    @ObservedObject var store: ConnectorsStore
    var onClose: () -> Void
    @State private var pick: String?
    @State private var env: [String: String] = [:]
    @State private var custom: [String: String] = [:]

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add an integration").font(Theme.h3)
            Text("MCP servers give Sam new tools. Anything that changes data asks for your OK first. Tokens are kept in the Keychain.").font(Theme.caption).foregroundStyle(Theme.text3)
            ScrollView {
                VStack(spacing: 8) {
                    ForEach(store.catalog) { t in row(t) }
                    customBox
                }
            }
            HStack {
                if store.busy { ProgressView().controlSize(.small); Text("Connecting…").font(Theme.caption).foregroundStyle(Theme.text3) }
                else if let m = store.message { Text(m.text).font(Theme.caption).foregroundStyle(m.ok ? Theme.good : Theme.warn).lineLimit(2) }
                Spacer()
                Button("Done", action: onClose).samButton(prominent: true).keyboardShortcut(.defaultAction)
            }
        }
        .padding(22)
        .frame(width: 560, height: 600)
        .environment(\.colorScheme, .dark)
    }

    private func row(_ t: CatalogItem) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: t.icon).foregroundStyle(Theme.accent).frame(width: 30, height: 30).background(RoundedRectangle(cornerRadius: Theme.chipRadius).fill(Theme.fill))
                VStack(alignment: .leading, spacing: 1) {
                    Text(t.name).font(Theme.body.weight(.semibold))
                    Text(t.description).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer()
                Button(pick == t.id ? "Cancel" : "Add") { pick = pick == t.id ? nil : t.id; env = [:] }.samButton(prominent: pick != t.id).controlSize(.small)
            }
            if pick == t.id { rowForm(t) }
        }
        .padding(10).background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
    }

    @ViewBuilder private func rowForm(_ t: CatalogItem) -> some View {
        ForEach(t.env, id: \.self) { e in secret(e) }
        Text(t.command).font(Theme.mono).foregroundStyle(Theme.text3).lineLimit(1)
        Button("Connect \(t.name)") { store.act(t.id, "add_mcp", ["env": env]); pick = nil }.samButton(prominent: true)
    }

    private func secret(_ e: String) -> some View {
        SecureField(e, text: Binding(get: { env[e] ?? "" }, set: { env[e] = $0 })).textFieldStyle(.roundedBorder)
    }

    private var envNames: [String] { (custom["env_names"] ?? "").split(whereSeparator: { $0 == "," || $0 == " " }).map(String.init) }

    private var customBox: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Custom MCP server").font(Theme.body.weight(.semibold))
            HStack {
                TextField("Name", text: bind("name")).textFieldStyle(.roundedBorder)
                TextField("Command (npx, uvx…)", text: bind("command")).textFieldStyle(.roundedBorder)
            }
            TextField("Arguments", text: bind("args")).textFieldStyle(.roundedBorder)
            TextField("Secret env var names (comma-separated)", text: bind("env_names")).textFieldStyle(.roundedBorder)
            ForEach(envNames, id: \.self) { e in secret(e) }
            Button("Add custom server") { addCustom() }.samButton()
        }
        .padding(10).background(RoundedRectangle(cornerRadius: Theme.tileRadius).fill(Theme.fill))
    }

    private func addCustom() {
        // A custom integration runs a program on this Mac: show the exact command and ask.
        let cmd = [custom["command"] ?? "", custom["args"] ?? ""].joined(separator: " ").trimmingCharacters(in: .whitespaces)
        let a = NSAlert()
        a.messageText = "Run this program as an integration?"
        a.informativeText = "Sam will start:\n\n\(cmd)\n\nOnly add servers you trust. Its tools ask for your OK before changing anything."
        a.alertStyle = .warning
        a.addButton(withTitle: "Add and run"); a.addButton(withTitle: "Cancel")
        guard a.runModal() == .alertFirstButtonReturn else { return }
        var v: [String: Any] = custom
        v["env"] = env
        store.act("custom", "add_mcp", v)
    }

    private func bind(_ k: String) -> Binding<String> { Binding(get: { custom[k] ?? "" }, set: { custom[k] = $0 }) }
}
