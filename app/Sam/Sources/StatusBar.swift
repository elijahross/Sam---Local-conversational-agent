// Menu-bar status: an icon with the number of running tasks (orange hand when something needs
// the user's OK), a glass drop-down with recent tasks, and the same drop-down as a short
// notification when a task finishes or needs an action.
import AppKit
import Combine
import SwiftUI

@MainActor
final class DropdownModel: ObservableObject {
    /// nil = list of recent tasks; a task id = notification about that task.
    @Published var toast: String?
    @Published var hovered = false
    @Published var voiceOff = false
}

@MainActor
final class StatusBar {
    let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    let store: TaskStore
    let dmodel = DropdownModel()
    private let panel: NSPanel
    private var host: NSHostingView<DropdownView>!
    private var bag = Set<AnyCancellable>()
    private var hideTask: Task<Void, Never>?
    private var clickMonitor: Any?
    var onOverview: ((String?) -> Void)?
    var onConnectors: (() -> Void)?
    var onTour: (() -> Void)?
    var onVoiceOff: (() -> Void)?
    var isOpen: Bool { panel.isVisible }

    init(store: TaskStore) {
        self.store = store
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 380, height: 200), styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.level = .statusBar
        panel.title = "Sam Tasks"
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary]
        host = NSHostingView(rootView: DropdownView(store: store, dmodel: dmodel,
                                                    onOpen: { [weak self] id in self?.hide(); self?.onOverview?(id) },
                                                    onConnectors: { [weak self] in self?.hide(); self?.onConnectors?() },
                                                    onTour: { [weak self] in self?.hide(); self?.onTour?() },
                                                    onVoiceOff: { [weak self] in self?.onVoiceOff?() },
                                                    onCancel: { [weak self] t, a in self?.store.confirm(task: t, action: a, decision: "cancel") }))
        panel.contentView = host
        if let b = item.button {
            b.target = self
            b.action = #selector(clicked)
            b.imagePosition = .imageLeading
            b.setAccessibilityLabel("Sam tasks")
        }
        store.$tasks.sink { [weak self] _ in Task { @MainActor in self?.refreshIcon(); self?.resize() } }.store(in: &bag)
        refreshIcon()
    }

    private func refreshIcon() {
        let r = store.running, n = store.needsAction
        // Sam's logo as a template image (macOS tints it for light and dark menu bars); a task
        // waiting for an OK shows an orange hand instead. Running tasks show their count.
        var img: NSImage? = SamLogo.templateImage(pointSize: 16)
        if n > 0, let i = NSImage(systemSymbolName: "hand.raised.fill", accessibilityDescription: "Sam tasks")?.withSymbolConfiguration(.init(pointSize: 13, weight: .semibold).applying(.init(paletteColors: [.systemOrange]))) {
            img = i; img?.isTemplate = false
        }
        item.button?.image = img
        item.button?.title = r + n > 0 ? " \(r + n)" : ""
        item.button?.toolTip = n > 0 ? "\(n) task(s) need your OK" : (r > 0 ? "\(r) task(s) running" : "Sam")
    }

    @objc private func clicked() {
        if panel.isVisible && dmodel.toast == nil { hide() } else { show(toast: nil) }
    }

    /// Opens the list (toast == nil) or a notification about one task (auto-hides after 6 s
    /// unless the pointer rests on it).
    func show(toast: String?) {
        hideTask?.cancel()
        dmodel.toast = toast
        resize()
        panel.orderFrontRegardless()
        if toast != nil {
            hideTask = Task { [weak self] in
                try? await Task.sleep(nanoseconds: 6_000_000_000)
                while let s = self, s.dmodel.hovered { try? await Task.sleep(nanoseconds: 500_000_000) }
                guard !Task.isCancelled, let self, self.dmodel.toast == toast else { return }
                self.hide()
            }
        }
        if clickMonitor == nil {
            clickMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown]) { [weak self] _ in
                Task { @MainActor in
                    guard let self, self.panel.isVisible, !self.panel.frame.contains(NSEvent.mouseLocation) else { return }
                    self.hide()
                }
            }
        }
    }

    func hide() {
        hideTask?.cancel()
        panel.orderOut(nil)
        if let m = clickMonitor { NSEvent.removeMonitor(m); clickMonitor = nil }
    }

    private func resize() {
        guard let bw = item.button?.window else { return }
        let size = host.fittingSize
        let screen = bw.screen ?? NSScreen.main ?? NSScreen.screens[0]
        var x = bw.frame.midX - size.width / 2
        x = min(max(x, screen.frame.minX + 8), screen.frame.maxX - size.width - 8)
        panel.setFrame(NSRect(x: x, y: bw.frame.minY - size.height - 4, width: size.width, height: size.height), display: true)
    }
}

struct DropdownView: View {
    @ObservedObject var store: TaskStore
    @ObservedObject var dmodel: DropdownModel
    var onOpen: (String?) -> Void
    var onConnectors: () -> Void
    var onTour: () -> Void = {}
    var onVoiceOff: () -> Void = {}
    var onCancel: (String, String) -> Void

    private var items: [TaskItem] {
        if let id = dmodel.toast { return store.tasks.filter { $0.id == id } }
        // Needs-you first, then running, then the most recent.
        let rank = ["needs_action": 0, "running": 1]
        return Array(store.tasks.enumerated().sorted { (rank[$0.element.status] ?? 2, $0.offset) < (rank[$1.element.status] ?? 2, $1.offset) }.map(\.element).prefix(6))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                SamLogoView(size: 15, color: Theme.text)
                Text(dmodel.toast == nil ? "Sam" : header).font(Theme.title).foregroundStyle(Theme.text)
                Spacer()
                if dmodel.toast == nil, store.running + store.needsAction > 0 {
                    Text(summary).font(Theme.caption).foregroundStyle(Theme.text2)
                }
                HeaderIcon(symbol: "questionmark", help: "Welcome tour: how to use Sam, permissions", action: onTour)
                HeaderIcon(symbol: "square.grid.2x2", help: "Connectors", action: onConnectors)
                HeaderIcon(symbol: "arrow.up.left.and.arrow.down.right", help: "Open the task overview") { onOpen(dmodel.toast) }
                HeaderIcon(symbol: dmodel.voiceOff ? "mic.slash.fill" : "mic.fill", help: dmodel.voiceOff ? "Voice is off (meetings): Sam neither listens nor speaks; results drop down here. Click to turn voice on." : "Turn voice off (meetings): no listening, no speaking; results drop down here.", action: onVoiceOff)
                Menu {
                    if Updates.shared.canCheck { Button("Check for Updates…") { Updates.shared.checkForUpdates() } }
                    if let note = Updates.shared.message { Text(note) }
                    Divider()
                    Button("Quit Sam") { NSApp.terminate(nil) }.keyboardShortcut("q", modifiers: .command)
                    Divider()
                    Button("Uninstall Sam…") { Uninstaller.confirmAndRun() }
                } label: {
                    Image(systemName: "power").font(Theme.caption.weight(.semibold)).frame(width: 26, height: 26)
                        .foregroundStyle(Theme.text2).background(Circle().fill(Theme.fillHover)).contentShape(Circle())
                }
                .menuStyle(.borderlessButton).menuIndicator(.hidden).frame(width: 26, height: 26)
                .help("Quit Sam (stops its background services), or uninstall it completely")
            }
            if !store.reachable && dmodel.toast == nil {
                Text("Sam's server isn't running.").font(Theme.body).foregroundStyle(Theme.text2)
            } else if items.isEmpty {
                Text("No tasks yet. Ask Sam to do something in the background.").font(Theme.body).foregroundStyle(Theme.text2)
            }
            ForEach(items) { t in
                DropdownRow(task: t, store: store, onOpen: { onOpen(t.id) }, onCancel: { a in onCancel(t.id, a) })
            }
            if dmodel.voiceOff, dmodel.toast == nil {
                Text("Voice is off: Sam neither listens nor speaks. Results drop down here.").font(Theme.caption).foregroundStyle(Theme.text3)
            }
        }
        .padding(14)
        .frame(width: 380)
        .samGlass(in: RoundedRectangle(cornerRadius: Theme.panelRadius, style: .continuous), tint: Theme.islandTint)
        .environment(\.colorScheme, .dark)   // legible over bright pages too
        .tint(Theme.accent)
        .padding(.horizontal, 10).padding(.bottom, 14)   // room for the glass edge
        .onHover { dmodel.hovered = $0 }
    }

    private var header: String {
        guard let t = items.first else { return "Sam" }
        return t.status == "needs_action" ? "Needs your OK" : (t.status == "done" ? "Done" : "Couldn't finish")
    }
    private var summary: String {
        [store.needsAction > 0 ? "\(store.needsAction) need you" : nil, store.running - store.queued > 0 ? "\(store.running - store.queued) running" : nil,
         store.queued > 0 ? "\(store.queued) queued" : nil].compactMap { $0 }.joined(separator: " · ")
    }
}

struct DropdownRow: View {
    let task: TaskItem
    @ObservedObject var store: TaskStore
    var onOpen: () -> Void
    var onCancel: (String) -> Void
    @State private var hover = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 10) {
                StatusGlyph(status: task.status, size: 15).padding(.top, 1)
                VStack(alignment: .leading, spacing: 2) {
                    Text(task.title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(2)
                    Text(task.pending.first?.summary ?? task.current).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(2)
                }
                Spacer(minLength: 4)
                // Finished / failed: a trash button on hover (waiting tasks have Cancel instead).
                if hover && (task.status == "done" || task.status == "failed") {
                    DeleteTaskButton(task: task, store: store).buttonStyle(.plain).foregroundStyle(Theme.text2)
                } else {
                    Text(TaskText.ago(task.createdDate)).font(Theme.caption).foregroundStyle(Theme.text3)
                }
            }
            if let a = task.pending.first {
                // Approving needs the full draft in view: Review opens it; Cancel is always safe.
                HStack(spacing: 8) {
                    Spacer().frame(width: 25)
                    Button("Review", action: onOpen).samButton(prominent: true).controlSize(.small)
                    Button("Cancel") { onCancel(a.id) }.samButton().controlSize(.small)
                    if task.pending.count > 1 { Text("+\(task.pending.count - 1) more").font(Theme.caption).foregroundStyle(Theme.text3) }
                }
            }
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: Theme.tileRadius, style: .continuous).fill((hover ? Theme.fill : Color.clear)))
        .contentShape(Rectangle())
        .onHover { hover = $0 }
        .onTapGesture(perform: onOpen)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }
}


/// Round, equally sized icon buttons for the dropdown header (SF Symbols of different widths
/// looked skewed inside text-sized capsules).
struct HeaderIcon: View {
    let symbol: String
    let help: String
    let action: () -> Void
    @State private var hover = false
    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(Theme.caption.weight(.semibold))
                .frame(width: 26, height: 26)
                .foregroundStyle(hover ? Theme.text : Theme.text2)
                .background(Circle().fill((hover ? Theme.fillSelected : Theme.fillHover)))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .help(help)
        .accessibilityLabel(help)
    }
}
