// The island window: a non-activating panel above the menu bar, fixed at its expanded size
// (SwiftUI animates inside it), click-through except where the island is drawn.
import AppKit
import SwiftUI

extension NSScreen {
    /// Hardware notch rect (macOS 12+ APIs), nil on screens without a notch.
    var notchRect: NSRect? {
        guard let l = auxiliaryTopLeftArea?.width, let r = auxiliaryTopRightArea?.width, safeAreaInsets.top > 0 else { return nil }
        let w = frame.width - l - r
        return NSRect(x: frame.midX - w / 2, y: frame.maxY - safeAreaInsets.top, width: w, height: safeAreaInsets.top)
    }
    var menuBarHeight: CGFloat { max(24, frame.maxY - visibleFrame.maxY) }
    static var builtin: NSScreen? {
        screens.first { s in
            guard let n = s.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID else { return false }
            return CGDisplayIsBuiltin(n) != 0
        }
    }
}

/// Geometry the views need: the notch (or a fake pill) size.
struct NotchGeometry: Equatable {
    var notchWidth: CGFloat
    var notchHeight: CGFloat
    var hasNotch: Bool
}

final class NotchPanel: NSPanel {
    init(rect: NSRect) {
        super.init(contentRect: rect, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        isFloatingPanel = true
        level = .mainMenu + 3
        collectionBehavior = [.canJoinAllSpaces, .stationary, .fullScreenAuxiliary, .ignoresCycle]
        backgroundColor = .clear
        isOpaque = false
        hasShadow = false
        isMovable = false
        hidesOnDeactivate = false
        becomesKeyOnlyIfNeeded = true
        isReleasedWhenClosed = false
        appearance = NSAppearance(named: .darkAqua)
    }
    /// Key only when a confirmation card needs ⏎/⎋ or text editing.
    var allowKey = false
    override var canBecomeKey: Bool { allowKey }
    override var canBecomeMain: Bool { false }
}

/// Hosting view that is click-through outside the island's current frame.
final class PassthroughHostingView<V: View>: NSHostingView<V> {
    var hitRect: () -> NSRect = { .zero }
    /// A click into the island lets it take the keyboard (editing card fields); nothing else does.
    override func mouseDown(with e: NSEvent) {
        if let p = window as? NotchPanel { p.allowKey = true; p.makeKey() }
        super.mouseDown(with: e)
    }
    override func hitTest(_ point: NSPoint) -> NSView? {
        hitRect().contains(point) ? super.hitTest(point) : nil
    }
}

@MainActor
final class NotchController {
    let panel: NotchPanel
    let model: IslandModel
    private(set) var geo: NotchGeometry
    private var host: PassthroughHostingView<IslandRoot>!
    var onHover: ((Bool) -> Void)?
    var onConfirm: ((String, String, [String: Any]) -> Void)?   // action id, decision, edited args
    var onOpen: ((String) -> Void)?
    var onClose: (() -> Void)?
    var onFix: ((Card) -> Void)?
    var onOverview: ((String?) -> Void)?
    var onToastDismiss: (() -> Void)?
    var onConnectors: (() -> Void)?
    var onTour: (() -> Void)?

    init(model: IslandModel) {
        self.model = model
        let screen = NSScreen.builtin ?? NSScreen.main ?? NSScreen.screens[0]
        geo = NotchController.geometry(screen)
        panel = NotchPanel(rect: NotchController.frame(screen))
        install()
        // Screens come and go (a display plugged in, the tests' virtual display). Hop to the main
        // actor with a Task rather than assumeIsolated: the notification can arrive from inside a
        // Swift-concurrency task whose executor is already gone, and assumeIsolated then crashed
        // (SIGSEGV in SerialExecutor._isSameExecutor, 2026-10-11).
        screenObserver = NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.relayout() }
        }
    }

    private var screenObserver: NSObjectProtocol?
    deinit {
        if let o = screenObserver { NotificationCenter.default.removeObserver(o) }
    }

    static func geometry(_ s: NSScreen) -> NotchGeometry {
        if let n = s.notchRect { return NotchGeometry(notchWidth: n.width, notchHeight: n.height, hasNotch: true) }
        return NotchGeometry(notchWidth: 190, notchHeight: s.menuBarHeight, hasNotch: false)
    }

    static func frame(_ s: NSScreen) -> NSRect {
        let w = Theme.panelWidth + 80, h = Theme.panelMaxHeight + 60
        return NSRect(x: s.frame.midX - w / 2, y: s.frame.maxY - h, width: w, height: h)
    }

    private func install() {
        let root = IslandRoot(model: model, geo: geo,
                              onHover: { [weak self] in self?.onHover?($0) },
                              onConfirm: { [weak self] in self?.onConfirm?($0, $1, $2) },
                              onOpen: { [weak self] in self?.onOpen?($0) },
                              onClose: { [weak self] in self?.onClose?() },
                              onFix: { [weak self] in self?.onFix?($0) },
                              onOverview: { [weak self] in self?.onOverview?($0) },
                              onConnectors: { [weak self] in self?.onConnectors?() },
                              onTour: { [weak self] in self?.onTour?() },
                              onToastDismiss: { [weak self] in self?.onToastDismiss?() })
        host = PassthroughHostingView(rootView: root)
        host.hitRect = { [weak self] in self?.islandRect() ?? .zero }
        host.frame = panel.contentView?.bounds ?? .zero
        host.autoresizingMask = [.width, .height]
        panel.contentView = host
        panel.orderFrontRegardless()
    }

    func relayout() {
        let s = NSScreen.builtin ?? NSScreen.main ?? NSScreen.screens[0]
        geo = NotchController.geometry(s)
        panel.setFrame(NotchController.frame(s), display: true)
        install()
    }

    /// The part of the panel that takes clicks (view coordinates, origin bottom-left).
    func islandRect() -> NSRect {
        let b = panel.contentView?.bounds ?? .zero
        let (w, h): (CGFloat, CGFloat)
        switch model.state {
        case .hidden: (w, h) = (geo.notchWidth + (model.live ? 96 : 0), geo.notchHeight + 4)   // the notch (+ live activity)
        case .toast: (w, h) = (Theme.panelWidth - 30, geo.notchHeight + 70)
        case .dictating: (w, h) = (Theme.panelWidth - 60, geo.notchHeight + 58)
        case .peek: (w, h) = (geo.notchWidth + 120, geo.notchHeight + 38)
        default: (w, h) = (Theme.panelWidth, min(Theme.panelMaxHeight, model.panelHeight + geo.notchHeight))
        }
        // The drawn surface flares into the top edge (IslandRoot.flare) and casts a shadow: the
        // clickable area covers the shoulders too, else a click on them fell through to the
        // app behind (2026-10-12).
        let flare: CGFloat = model.state == .hidden ? 0 : (model.state == .peek ? 8 : 14)
        let (cw, ch) = (w + flare * 2, h + (model.state == .hidden ? 0 : 6))
        return NSRect(x: b.midX - cw / 2, y: b.maxY - ch, width: cw, height: ch)
    }

    /// Screen rect of the island (tests and click-outside detection).
    func islandScreenRect() -> NSRect {
        let r = islandRect()
        return NSRect(x: panel.frame.minX + r.minX, y: panel.frame.minY + r.minY, width: r.width, height: r.height)
    }
}
