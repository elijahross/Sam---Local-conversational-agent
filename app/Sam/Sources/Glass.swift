// Translucent surfaces: dark Liquid Glass on macOS 26, a behind-window blur before, and a solid
// fill when the user turned on Reduce Transparency.
import AppKit
import SwiftUI

struct BackdropBlur: NSViewRepresentable {
    var material: NSVisualEffectView.Material = .hudWindow
    func makeNSView(context: Context) -> NSVisualEffectView {
        let v = NSVisualEffectView()
        v.material = material
        v.blendingMode = .behindWindow
        v.state = .active
        v.appearance = NSAppearance(named: .darkAqua)
        return v
    }
    func updateNSView(_ v: NSVisualEffectView, context: Context) { v.material = material }
}

enum GlassPrefs {
    static var reduceTransparency: Bool { NSWorkspace.shared.accessibilityDisplayShouldReduceTransparency }
}

extension View {
    /// Dark glass behind a surface (island, dropdown, overview panes).
    @ViewBuilder func samGlass<S: Shape>(in shape: S, tint: Color = Theme.glassTint) -> some View {
        if GlassPrefs.reduceTransparency {
            self.background(shape.fill(Theme.card))
        } else if #available(macOS 26, *) {
            self.glassEffect(.regular.tint(tint), in: shape)
        } else {
            self.background(ZStack { BackdropBlur(); tint }.clipShape(shape))
        }
    }

    /// Glass button on macOS 26 (prominent = the primary action), bordered before.
    @ViewBuilder func samButton(prominent: Bool = false) -> some View {
        if #available(macOS 26, *) {
            if prominent { self.buttonStyle(.glassProminent) } else { self.buttonStyle(.glass) }
        } else {
            if prominent { self.buttonStyle(.borderedProminent) } else { self.buttonStyle(.bordered) }
        }
    }
}

/// The app's one background: a deep blue-violet field with two soft glows. Onboarding, the
/// connectors window and the overview all sit on it (2026-10-14: three different backgrounds before).
struct AppBackdrop: View {
    var body: some View {
        ZStack {
            Theme.bgDeep
            RadialGradient(colors: [Theme.accent.opacity(0.35), .clear], center: .topLeading, startRadius: 10, endRadius: 520)
            RadialGradient(colors: [Theme.thinkB.opacity(0.22), .clear], center: .bottomTrailing, startRadius: 10, endRadius: 560)
        }
        .ignoresSafeArea()
    }
}

/// The one card surface: glass fill, hairline stroke, a 1 pt light catch along the top edge,
/// continuous corners. Cards in the island, the overview, the connectors grid and the report tiles.
extension View {
    func samCard(radius: CGFloat = Theme.cardRadius, tone: Color? = nil) -> some View {
        let shape = RoundedRectangle(cornerRadius: radius, style: .continuous)
        return self
            .background(shape.fill(tone.map { $0.opacity(0.10) } ?? Theme.fill))
            .overlay(shape.strokeBorder(tone.map { $0.opacity(0.35) } ?? Theme.stroke))
            .overlay(alignment: .top) {
                LinearGradient(colors: [Color.white.opacity(0.10), .clear], startPoint: .top, endPoint: .bottom)
                    .frame(height: 1).padding(.horizontal, radius * 0.8)
            }
            .clipShape(shape)
    }
}

/// A symbol on a soft accent square: the leading glyph of cards, connectors and detail headers.
struct IconTile: View {
    let symbol: String
    var size: CGFloat = 32
    var tone: Color = Theme.accent
    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: size * 0.46, weight: .semibold))
            .foregroundStyle(tone)
            .frame(width: size, height: size)
            .background(RoundedRectangle(cornerRadius: size * 0.3, style: .continuous).fill(tone.opacity(0.16)))
    }
}
