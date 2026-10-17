// Task overview: a full-screen blurred backdrop with the task list on the left and the selected
// task in the middle: what was asked, what's happening now, the plan, every step, files and
// attachments, approvals waiting, and the result. Esc, ✕ or a click on the backdrop closes it.
import AppKit
import SwiftUI
import UniformTypeIdentifiers

final class OverviewPanel: NSPanel {
    override var canBecomeKey: Bool { true }   // Esc and text fields in approval cards
}

/// Where the Dock and menu bar are on the overview's screen: the panes stay clear of them (the
/// backdrop still covers the whole screen).
final class OverviewLayout: ObservableObject {
    @Published var insets = EdgeInsets()
    /// Recent · History · Monitors.
    @Published var tab: OverviewTab = .recent
    /// The You tab's sidebar index asks the main pane to scroll here.
    @Published var youSection: YouSection?
    static func insets(_ s: NSScreen) -> EdgeInsets {
        let f = s.frame, v = s.visibleFrame
        return EdgeInsets(top: f.maxY - v.maxY, leading: v.minX - f.minX, bottom: v.minY - f.minY, trailing: f.maxX - v.maxX)
    }
}

@MainActor
final class OverviewController {
    let store: TaskStore
    let layout = OverviewLayout()
    let history: HistoryStore
    let monitors: MonitorStore
    let friend: FriendStore
    private var panel: OverviewPanel?
    private var keyMonitor: Any?
    var isOpen: Bool { panel?.isVisible ?? false }
    var onOpenFile: ((String) -> Void)?
    var onConnectors: (() -> Void)?

    init(store: TaskStore) {
        self.store = store
        history = HistoryStore(base: store.base)
        monitors = MonitorStore(base: store.base)
        friend = FriendStore(base: store.base)
        store.onDeleted = { [weak history] id in history?.remove(id) }
    }

    /// `screen`: where to show it (tests use Sam's virtual display); default = the screen
    /// with the pointer.
    /// `monitor`: open the Monitors tab on that monitor (a monitor alert's card).
    func show(task: String?, monitor: String? = nil, screen: NSScreen? = nil) {
        let scr = screen ?? NSScreen.screens.first { $0.frame.contains(NSEvent.mouseLocation) } ?? NSScreen.main ?? NSScreen.screens[0]
        if panel == nil {
            let p = OverviewPanel(contentRect: scr.frame, styleMask: [.borderless], backing: .buffered, defer: false)
            p.isOpaque = false
            p.backgroundColor = .clear
            p.level = .floating
            p.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            p.isReleasedWhenClosed = false
            p.isRestorable = false
            p.title = "Sam Overview"
            p.contentView = NSHostingView(rootView: OverviewView(store: store, history: history, monitors: monitors, friend: friend, layout: layout, onClose: { [weak self] in self?.hide() }, onOpenFile: { [weak self] f in self?.onOpenFile?(f) },
                                                                 onConnectors: { [weak self] in self?.hide(); self?.onConnectors?() }))
            panel = p
        }
        panel?.setFrame(scr.frame, display: true)
        layout.insets = OverviewLayout.insets(scr)
        store.fastPoll = true
        if let monitor {
            layout.tab = .monitors
            monitors.selected = monitor
        } else {
            // A specific task opens where it lives (Recent if it's still there, else History).
            if let task, !store.tasks.contains(where: { $0.id == task }) { layout.tab = .history } else if task != nil { layout.tab = .recent }
            store.selected = task ?? store.tasks.first(where: { $0.status == "needs_action" })?.id ?? store.tasks.first?.id
        }
        NSApp.activate(ignoringOtherApps: true)   // the user asked for it: take the keyboard for Esc / edits
        panel?.makeKeyAndOrderFront(nil)
        if keyMonitor == nil {
            keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
                if e.keyCode == 53, self?.isOpen == true { MainActor.assumeIsolated { self?.hide() }; return nil }
                return e
            }
        }
    }

    func hide() {
        panel?.orderOut(nil)
        store.fastPoll = false
        if let m = keyMonitor { NSEvent.removeMonitor(m); keyMonitor = nil }
    }
}

struct OverviewView: View {
    @ObservedObject var store: TaskStore
    @ObservedObject var history: HistoryStore
    @ObservedObject var monitors: MonitorStore
    @ObservedObject var friend: FriendStore
    @ObservedObject var layout: OverviewLayout
    var onClose: () -> Void
    var onOpenFile: (String) -> Void
    var onConnectors: () -> Void = {}

    var body: some View {
        ZStack {
            Group {
                if GlassPrefs.reduceTransparency { Theme.bgDeep.opacity(0.92) } else { BackdropBlur(material: .fullScreenUI) }
            }
            .ignoresSafeArea()
            // Darker than the blur alone: the panes read clearly over busy windows.
            // The same field as the onboarding and connectors windows, over the blurred desktop.
            AppBackdrop().opacity(0.86).contentShape(Rectangle()).onTapGesture(perform: onClose)
            HStack(alignment: .top, spacing: 20) {
                TaskSidebar(store: store, history: history, monitors: monitors, friend: friend, layout: layout, onClose: onClose, onConnectors: onConnectors).frame(width: 320)
                if layout.tab == .monitors {
                    if monitors.selected?.hasPrefix("m") == true || monitors.automation == nil && monitors.campaign == nil {
                        MonitorDetail(monitors: monitors).frame(maxWidth: 820)
                    } else {
                        AutomationDetail(monitors: monitors).frame(maxWidth: 820)
                    }
                } else if layout.tab == .you {
                    YouDetail(f: friend, layout: layout).frame(maxWidth: 820)
                } else {
                    TaskDetail(store: store, onOpenFile: onOpenFile, onOpenMonitor: { id in layout.tab = .monitors; monitors.selected = id }).frame(maxWidth: 820)
                }
            }
            .padding(.leading, 48 + layout.insets.leading).padding(.trailing, 48 + layout.insets.trailing)
            .padding(.top, 24 + max(layout.insets.top, 40)).padding(.bottom, 24 + max(layout.insets.bottom, 24))
            .frame(maxWidth: 1240 + layout.insets.leading + layout.insets.trailing)
        }
        .environment(\.colorScheme, .dark)
        .tint(Theme.accent)
    }
}

struct TaskSidebar: View {
    @ObservedObject var store: TaskStore
    @ObservedObject var history: HistoryStore
    @ObservedObject var monitors: MonitorStore
    @ObservedObject var friend: FriendStore
    @ObservedObject var layout: OverviewLayout
    @State private var hovered: String?
    var onClose: () -> Void
    var onConnectors: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Tasks").font(Theme.h2).foregroundStyle(Theme.text)
                Spacer()
                Button(action: onClose) { Image(systemName: "xmark").font(.system(size: 11, weight: .bold)) }
                    .samButton().controlSize(.small).accessibilityLabel("Close overview").keyboardShortcut(.cancelAction)
            }
            Picker("View", selection: $layout.tab) { ForEach(OverviewTab.allCases) { Text($0.rawValue).tag($0) } }
                .pickerStyle(.segmented).labelsHidden().accessibilityLabel("Overview view")
            switch layout.tab {
            case .recent:
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        section("Needs you", store.tasks.filter { $0.status == "needs_action" })
                        section("Running", store.tasks.filter { $0.status == "running" })
                        section("Queued", store.tasks.filter { $0.status == "queued" })
                        section("Last 24 hours", store.tasks.filter { $0.status == "done" || $0.status == "failed" }, clear: true)
                        if store.tasks.isEmpty {
                            Text("Nothing in the last 24 hours.").font(Theme.body).foregroundStyle(Theme.text2)
                            Button("Search History") { layout.tab = .history }.buttonStyle(.plain).font(Theme.caption).foregroundStyle(Theme.accent)
                        }
                    }
                }
                .scrollIndicators(.never)
            case .history:
                HistoryList(history: history, store: store)
            case .monitors:
                MonitorList(monitors: monitors)
            case .you:
                YouSidebar(f: friend, layout: layout)
            }
            Spacer(minLength: 0)
            Button(action: onConnectors) { Label("Connectors", systemImage: "square.grid.2x2").frame(maxWidth: .infinity) }.samButton()
        }
        .padding(16)
        .frame(maxHeight: .infinity, alignment: .top)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
    }

    @ViewBuilder private func section(_ title: String, _ items: [TaskItem], clear: Bool = false) -> some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(title.uppercased()).font(Theme.overline).foregroundStyle(Theme.text3).padding(.leading, 8)
                    Spacer()
                    if clear {
                        Button("Clear") { store.clearFinished() }.buttonStyle(.plain).font(Theme.overline).foregroundStyle(Theme.text3)
                            .help("Clear finished and failed tasks from this list (they stay in History)").accessibilityLabel("Clear finished tasks").padding(.trailing, 8)
                    }
                }
                ForEach(items) { t in
                    Button { store.selected = t.id } label: {
                        HStack(alignment: .top, spacing: 9) {
                            StatusGlyph(status: t.status).padding(.top, 1)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(t.title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                                if !t.headline.isEmpty && t.requestLine.lowercased() != t.headline.lowercased() {
                                    Text("“\(t.requestLine)”").font(Theme.caption).italic().foregroundStyle(Theme.text3).lineLimit(1)
                                }
                                Text(TaskText.ago(t.createdDate) + (t.context.isEmpty ? "" : " · \(t.context)")).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                            }
                            Spacer(minLength: 0)
                        }
                        .padding(8)
                        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill((store.selected == t.id ? Theme.fillSelected : Color.clear)))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .onHover { inside in if inside { hovered = t.id } else if hovered == t.id { hovered = nil } }
                    .overlay(alignment: .topTrailing) {
                        // The trash button appears on hover / selection (keeps the list calm).
                        if TaskStore.deletable(t) && (hovered == t.id || store.selected == t.id) {
                            DeleteTaskButton(task: t, store: store).buttonStyle(.plain).foregroundStyle(Theme.text2).padding(6)
                                .background(Circle().fill(Theme.glassTint)).padding(4)
                        }
                    }
                }
            }
        }
    }
}

struct TaskDetail: View {
    @ObservedObject var store: TaskStore
    var onOpenFile: (String) -> Void
    var onOpenMonitor: (String) -> Void = { _ in }
    @State private var expanded: Set<Int> = []
    @State private var showWork = false

    var body: some View {
        Group {
            if let t = store.detail {
                ScrollView {
                    VStack(alignment: .leading, spacing: 22) {
                        header(t)
                        if !t.cards.isEmpty {
                            block("Waiting for you") {
                                ForEach(t.cards) { c in
                                    CardView(card: c, onConfirm: { aid, decision, args in store.confirm(task: t.id, action: aid, decision: decision, args: args) },
                                             onOpen: onOpenFile, onFix: { _ in })
                                }
                            }
                        }
                        if let rep = t.report {
                            // The presented result first; how it was made folds away below.
                            ReportView(r: rep, onOpen: onOpenFile)
                            if let mid = rep["monitor"] as? String {
                                Button { onOpenMonitor(mid) } label: { Label("Open the monitor", systemImage: "dot.radiowaves.left.and.right") }.samButton()
                            }
                            DisclosureGroup(isExpanded: $showWork) {
                                VStack(alignment: .leading, spacing: 22) { work(t) }.padding(.top, 12)
                            } label: {
                                Text("How Sam got there — steps, files, raw result").font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text2)
                            }
                        } else if let v = t.view, t.status == "done" {
                            // A quick task's typed result, large; the raw line folds away below.
                            TypedResultView(view: v, large: true)
                                .frame(maxWidth: 560, alignment: .leading)
                                .samCard(radius: Theme.paneRadius - 6)
                            DisclosureGroup(isExpanded: $showWork) {
                                VStack(alignment: .leading, spacing: 22) { work(t) }.padding(.top, 12)
                            } label: {
                                Text("How Sam got there — steps and raw result").font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text2)
                            }
                        } else {
                            if t.reportStatus == "building" {
                                HStack(spacing: 10) {
                                    ProgressView().controlSize(.small)
                                    Text("Preparing the overview — summary, highlights, pictures…").font(Theme.body).foregroundStyle(Theme.text2)
                                }
                                .padding(12).frame(maxWidth: .infinity, alignment: .leading)
                                .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
                            }
                            work(t)
                        }
                        if !t.notes.isEmpty {
                            block("Notes") { ForEach(Array(t.notes.enumerated()), id: \.offset) { _, n in RichText(text: n) } }
                        }
                    }
                    .padding(28)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .scrollIndicators(.never)
            } else {
                VStack(spacing: 10) {
                    Image(systemName: "sparkles").font(.system(size: 30)).foregroundStyle(Theme.text3)
                    Text(store.tasks.isEmpty ? "No tasks yet" : "Pick a task").font(Theme.title).foregroundStyle(Theme.text2)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.paneRadius, style: .continuous), tint: Theme.glassTint)
    }

    @ViewBuilder private func work(_ t: TaskItem) -> some View {
        if !t.steps.isEmpty { block("Steps") { timeline(t) } }
        if !t.files.isEmpty { block("Files and attachments") { ForEach(t.files, id: \.self) { FileRow(path: $0, onOpen: onOpenFile) } } }
        if !t.result.isEmpty && (t.status == "done" || t.status == "failed") {
            block("Result") {
                RichText(text: t.result)
            }
        }
    }

    private func header(_ t: TaskItem) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Pill(text: TaskText.status(t.status), color: t.status == "done" ? Theme.good : t.status == "failed" ? Theme.bad : t.status == "needs_action" ? .orange : Theme.accent)
                if !t.context.isEmpty { Chip(icon: "scope", text: t.context) }
                Spacer()
                Text(t.createdDate.formatted(date: .abbreviated, time: .shortened)).font(Theme.caption).foregroundStyle(Theme.text3)
                if TaskStore.deletable(t) { DeleteTaskButton(task: t, store: store, label: true).samButton().controlSize(.small) }
            }
            Text(t.title).font(Theme.h2).foregroundStyle(Theme.text).textSelection(.enabled)
            if !t.headline.isEmpty && t.requestLine.lowercased() != t.headline.lowercased() {
                // The user's own words, small and quoted, under the headline.
                Text("“\(t.requestLine)”").font(Theme.body).italic().foregroundStyle(Theme.text3).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                StatusGlyph(status: t.status)
                Text(t.current).font(Theme.body).foregroundStyle(Theme.text2).lineLimit(2)
            }
            if !t.plan.isEmpty {
                HStack(spacing: 6) {
                    Text("Plan").font(Theme.caption).foregroundStyle(Theme.text3)
                    ForEach(Array(t.plan.enumerated()), id: \.offset) { i, p in
                        if i > 0 { Image(systemName: "chevron.right").font(.system(size: 8)).foregroundStyle(Theme.text3) }
                        Text(TaskText.tool(p)).font(Theme.caption).foregroundStyle(Theme.text2)
                    }
                    if t.tokens > 0 { Spacer(); Text("\(t.tokens) tokens").font(Theme.caption).foregroundStyle(Theme.text3) }
                }
            }
        }
    }

    private func timeline(_ t: TaskItem) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(t.steps) { s in
                HStack(alignment: .top, spacing: 12) {
                    VStack(spacing: 0) {
                        StatusGlyph(status: s.status).frame(width: 18, height: 18)
                        if s.id < t.steps.count - 1 { Rectangle().fill(Theme.stroke).frame(width: 1.5).frame(maxHeight: .infinity) }
                    }
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Text(TaskText.tool(s.tool)).font(Theme.body.weight(.semibold)).foregroundStyle(Theme.text)
                            if s.at > 0 { Text(Date(timeIntervalSince1970: s.at / 1000).formatted(date: .omitted, time: .standard)).font(Theme.caption).foregroundStyle(Theme.text3) }
                        }
                        if !s.summary.isEmpty {
                            if expanded.contains(s.id) {
                                RichText(text: s.summary)
                            } else {
                                Text(plainPreview(s.summary)).font(Theme.body).foregroundStyle(Theme.text2).lineLimit(3).frame(maxWidth: .infinity, alignment: .leading)
                            }
                            Button(expanded.contains(s.id) ? "Show less" : "Show more") { if expanded.contains(s.id) { expanded.remove(s.id) } else { expanded.insert(s.id) } }
                                .buttonStyle(.plain).font(Theme.caption).foregroundStyle(Theme.accent)
                        }
                        if let f = s.file { FileRow(path: f, onOpen: onOpenFile, compact: true) }
                    }
                    .padding(.bottom, 14)
                }
            }
        }
    }

    private func block<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(Theme.label).foregroundStyle(Theme.text3)
            content()
        }
    }
}

struct Pill: View {
    let text: String
    let color: Color
    var body: some View {
        Text(text).font(Theme.caption.weight(.semibold)).foregroundStyle(color)
            .padding(.horizontal, 9).padding(.vertical, 3)
            .background(Capsule().fill(color.opacity(0.16)))
            .overlay(Capsule().strokeBorder(color.opacity(0.25)))
    }
}

struct FileRow: View {
    let path: String
    var onOpen: (String) -> Void
    var compact = false
    @State private var preview = false
    @State private var text: String?

    private var ext: String { RichKind.ext(path) }
    private var canPreview: Bool { RichKind.isImage(path) || RichKind.textExts.contains(ext) }
    /// The type's icon (no disk access: the app can't read ~/Documents without a prompt).
    private var icon: NSImage {
        let isDir = !path.contains(".") || path.hasSuffix("/")
        return NSWorkspace.shared.icon(for: isDir ? .folder : (UTType(filenameExtension: ext) ?? .data))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(nsImage: icon).resizable().frame(width: compact ? 18 : 26, height: compact ? 18 : 26)
                VStack(alignment: .leading, spacing: 1) {
                    Text((path as NSString).lastPathComponent).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(1)
                    if !compact { Text((path as NSString).deletingLastPathComponent).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.middle) }
                }
                Spacer()
                if canPreview {
                    Button(preview ? "Hide" : "Preview") { preview.toggle() }.samButton().controlSize(.small)
                }
                Button("Open") { onOpen(path) }.samButton().controlSize(.small)
                Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) } label: { Image(systemName: "folder") }
                    .samButton().controlSize(.small).help("Show in Finder").accessibilityLabel("Show in Finder")
            }
            if preview {
                if RichKind.isImage(path) {
                    Thumb(src: path)
                } else if let text {
                    ScrollView { RichText(text: ext == "json" || ext == "jsonl" ? text : text) }.frame(maxHeight: 360)
                } else {
                    ProgressView().controlSize(.small).task {
                        let d = await Remote.file(path)
                        text = d.flatMap { String(data: $0.prefix(200_000), encoding: .utf8) } ?? "Couldn't load this file."
                    }
                }
            }
        }
        .padding(compact ? 6 : 10)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill(Theme.fill))
    }
}
