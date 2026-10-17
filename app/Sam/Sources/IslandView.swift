// Island views (design-system.md §3–4): notch pill → peek → panel with orb, waveform,
// transcript/answer, cards and confirmation cards.
import SwiftUI

struct HeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}

/// Notch-shaped silhouette: fused with the screen edge at the top, rounded bottom. `flare` adds
/// concave shoulders where it meets the top edge (inside the rect: the body is inset by it), so
/// the open island looks pulled out of the frame like a drop of water.
struct IslandShape: Shape {
    var bottomRadius: CGFloat
    var flare: CGFloat = 0
    var animatableData: AnimatablePair<CGFloat, CGFloat> {
        get { AnimatablePair(bottomRadius, flare) }
        set { bottomRadius = newValue.first; flare = newValue.second }
    }
    func path(in r: CGRect) -> Path {
        var p = Path()
        let f = max(0, min(flare, r.width / 4, r.height / 2))
        let (l, rt) = (r.minX + f, r.maxX - f)
        let br = min(bottomRadius, r.height / 2, (rt - l) / 2)
        p.move(to: CGPoint(x: r.minX, y: r.minY))
        p.addLine(to: CGPoint(x: r.maxX, y: r.minY))
        // right shoulder: from the top edge curving in and down into the body's side
        p.addCurve(to: CGPoint(x: rt, y: r.minY + f * 1.6), control1: CGPoint(x: rt + f * 0.45, y: r.minY), control2: CGPoint(x: rt, y: r.minY + f * 0.55))
        p.addLine(to: CGPoint(x: rt, y: r.maxY - br))
        p.addQuadCurve(to: CGPoint(x: rt - br, y: r.maxY), control: CGPoint(x: rt, y: r.maxY))
        p.addLine(to: CGPoint(x: l + br, y: r.maxY))
        p.addQuadCurve(to: CGPoint(x: l, y: r.maxY - br), control: CGPoint(x: l, y: r.maxY))
        p.addLine(to: CGPoint(x: l, y: r.minY + f * 1.6))
        p.addCurve(to: CGPoint(x: r.minX, y: r.minY), control1: CGPoint(x: l, y: r.minY + f * 0.55), control2: CGPoint(x: l - f * 0.45, y: r.minY))
        p.closeSubpath()
        return p
    }
}

struct IslandRoot: View {
    @ObservedObject var model: IslandModel
    let geo: NotchGeometry
    var onHover: (Bool) -> Void
    var onConfirm: (String, String, [String: Any]) -> Void
    var onOpen: (String) -> Void
    var onClose: () -> Void
    var onFix: (Card) -> Void
    var onOverview: (String?) -> Void = { _ in }
    var onConnectors: () -> Void = {}
    var onTour: () -> Void = {}
    var onToastDismiss: () -> Void = {}
    @Environment(\.accessibilityReduceMotion) private var reduce

    private var size: CGSize {
        switch model.state {
        case .hidden: return CGSize(width: geo.notchWidth + (model.live ? 96 : 0), height: geo.notchHeight)
        case .toast: return CGSize(width: Theme.panelWidth - 30, height: geo.notchHeight + 70)
        case .dictating: return CGSize(width: Theme.panelWidth - 60, height: geo.notchHeight + 58)
        case .peek: return CGSize(width: geo.notchWidth + 120, height: geo.notchHeight + 34)
        default: return CGSize(width: Theme.panelWidth, height: min(Theme.panelMaxHeight, geo.notchHeight + model.panelHeight))
        }
    }

    /// Shoulder size: open surfaces flare into the top edge; the resting pill doesn't.
    private var flare: CGFloat {
        switch model.state {
        case .hidden: return 0
        case .peek: return 8
        default: return 14
        }
    }
    private var bottomRadius: CGFloat {
        model.expanded || model.state == .toast || model.state == .dictating ? Theme.panelRadius : (model.state == .peek ? 16 : 10)
    }

    var body: some View {
        VStack(spacing: 0) {
            ZStack(alignment: .top) {
                islandBackground
                    .shadow(color: .black.opacity(model.expanded ? 0.45 : 0), radius: 18, y: 8)
                    .opacity(model.state == .hidden && geo.hasNotch ? 1 : (model.state == .hidden ? (model.live ? 1 : 0) : 1))
                if model.state == .hidden {
                    liveActivity
                } else {
                    content
                        .padding(.top, geo.notchHeight + (model.expanded ? 6 : 0))
                        .padding(.horizontal, flare)
                }
            }
            .frame(width: size.width + flare * 2, height: size.height)
            .clipShape(IslandShape(bottomRadius: bottomRadius, flare: flare))
            .contentShape(Rectangle())
            .onHover(perform: onHover)
            .animation(Theme.shape(reduce), value: model.state)
            .animation(Theme.content(reduce), value: model.cards.map(\.id))
            if let chip = model.taskChip, !model.expanded {
                Chip(icon: "hourglass", text: chip).padding(.top, 6).transition(.opacity)
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .environment(\.colorScheme, .dark)
        .tint(Theme.accent)
    }

    /// Hidden or peeking it's pure black (it continues the hardware notch); open it's dark glass,
    /// still black where it meets the notch.
    /// Hidden with background tasks: a spinner (or an orange hand when something needs you) left
    /// of the notch and the count right of it, like a Dynamic Island live activity. Click = overview.
    @ViewBuilder private var liveActivity: some View {
        if model.live {
            HStack(spacing: 0) {
                Group {
                    if model.liveNeeds > 0 { Image(systemName: "hand.raised.fill").foregroundStyle(.orange).font(Theme.caption.weight(.semibold)) }
                    else { ProgressView().controlSize(.mini).tint(Theme.accent) }
                }
                .frame(width: 48)
                Spacer(minLength: geo.notchWidth)
                Text("\(model.liveRunning + model.liveNeeds)").font(.system(size: 12, weight: .semibold, design: .rounded).monospacedDigit())
                    .foregroundStyle(model.liveNeeds > 0 ? .orange : Theme.text).frame(width: 48)
            }
            .frame(width: size.width, height: geo.notchHeight)
            .contentShape(Rectangle())
            .onTapGesture { onOverview(nil) }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(model.liveNeeds > 0 ? "\(model.liveNeeds) task(s) need your OK" : "\(model.liveRunning) task(s) running")
            .accessibilityAddTraits(.isButton)
            .transition(.opacity)
        }
    }

    @ViewBuilder private var islandBackground: some View {
        let shape = IslandShape(bottomRadius: bottomRadius, flare: flare)
        if model.state != .hidden && !GlassPrefs.reduceTransparency {
            // Same material as the menu-bar dropdown (glass, 55 % dark tint); only a short black
            // fade at the top so it still fuses with the hardware notch.
            ZStack(alignment: .top) {
                // The window server hit-tests a non-opaque window by its own pixels; Liquid Glass is
                // composited by the system, leaving them at alpha 0 and clicks fell through to the
                // app behind (2026-10-13). A dark base fill inside the shape makes the island solid
                // to the mouse and looks the same under the 55 % tint.
                shape.fill(Theme.island.opacity(0.42))
                Color.clear.samGlass(in: shape, tint: Theme.islandTint)
                LinearGradient(colors: [.black, .black.opacity(0)], startPoint: .top, endPoint: .bottom)
                    .frame(height: geo.notchHeight).clipShape(shape)
            }
        } else {
            shape.fill(Theme.island)
        }
    }

    @ViewBuilder private var content: some View {
        switch model.state {
        case .hidden: EmptyView()
        case .peek:
            HStack(spacing: 8) {
                Orb(model: model, size: 18)
                if model.live {
                    Button { onOverview(nil) } label: {
                        Text(model.liveNeeds > 0 ? "\(model.liveNeeds) need your OK · open" : "\(model.liveRunning) running · open").font(Theme.caption).foregroundStyle(model.liveNeeds > 0 ? .orange : Theme.text2)
                    }.buttonStyle(.plain)
                } else {
                    Text(model.connected ? "Hold fn to speak · ⌥Space for hands-free" : "Connecting…").font(Theme.caption).foregroundStyle(Theme.text2)
                }
            }
            .padding(.top, 4)
        case .dictating:
            HStack(spacing: 10) {
                Image(systemName: "text.cursor").font(Theme.title).foregroundStyle(Theme.listening)
                VStack(alignment: .leading, spacing: 3) {
                    if let r = model.dictationResult {
                        Text(r).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text)
                    } else {
                        Waveform(bands: model.bands, color: Theme.listening).frame(height: 18)
                    }
                    Text(model.dictationResult == nil ? "Dictating into \(model.dictationTarget ?? "the focused field") · release to insert" : (model.dictationTarget ?? ""))
                        .font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16).padding(.top, 4)
        case .toast:
            if let t = model.toast {
                HStack(alignment: .center, spacing: 10) {
                    StatusGlyph(status: t.status, size: 18)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(t.status == "needs_action" ? "Needs your OK" : (t.status == "done" ? "Done" : "Couldn't finish")).font(Theme.caption).foregroundStyle(t.status == "needs_action" ? .orange : Theme.text3)
                        Text(t.title).font(Theme.body.weight(.medium)).foregroundStyle(Theme.text).lineLimit(1)
                        Text(t.pending.first?.summary ?? t.current).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1)
                    }
                    Spacer(minLength: 4)
                    Button("Review") { onOverview(t.id) }.samButton(prominent: t.status == "needs_action").controlSize(.small)
                    Button { onToastDismiss() } label: { Image(systemName: "xmark").font(.system(size: 9, weight: .bold)) }
                        .samButton().controlSize(.small).accessibilityLabel("Dismiss")
                }
                .padding(.horizontal, 14).padding(.top, 2)
            }
        default:
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 12) {
                    Orb(model: model, size: 32)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(statusLine).font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                    Spacer()
                    if let c = model.contextChip { Chip(icon: "scope", text: c) }
                    Menu {
                        Picker("Voice", selection: $model.voiceChoice) {
                            Text("Sam (male)").tag("sam-male")
                            Text("Sam (female)").tag("sam-female")
                        }
                        .pickerStyle(.inline)
                        Divider()
                        Toggle(isOn: $model.voiceOff) { Label("Voice off (meetings)", systemImage: "mic.slash") }
                        Divider()
                        Section("Share with your question") {
                            Toggle(isOn: Binding(get: { ContextCapture.highlightAsk }, set: { ContextCapture.highlightAsk = $0 })) { Label("Highlighted text", systemImage: "text.cursor") }
                            Toggle(isOn: Binding(get: { ContextCapture.focusAsk }, set: { ContextCapture.focusAsk = $0 })) { Label("Focused window", systemImage: "macwindow") }
                        }
                        Divider()
                        Button("Tasks…") { onOverview(nil) }
                        Button("Connectors…") { onConnectors() }
                        Button("Welcome tour…") { onTour() }
                    } label: {
                        Image(systemName: model.voiceOff ? "mic.slash" : "person.wave.2").font(.system(size: 10, weight: .bold)).foregroundStyle(model.voiceOff ? Theme.warn : Theme.text3)
                    }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                    .accessibilityLabel("Choose voice")
                    Button(action: onClose) { Image(systemName: "xmark").font(.system(size: 10, weight: .bold)).foregroundStyle(Theme.text3) }
                        .buttonStyle(.plain).accessibilityLabel("Close Sam")
                }
                if !model.transcript.isEmpty && (model.state == .listening || model.state == .thinking) {
                    Text(model.transcript).font(Theme.caption).foregroundStyle(Theme.text2)
                        .lineLimit(2).truncationMode(.head).frame(maxWidth: .infinity, alignment: .leading)
                }
                if !model.answer.isEmpty && model.state != .listening {
                    ScrollView { Text(model.answer).font(Theme.body).foregroundStyle(Theme.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                        .frame(maxHeight: 64)
                }
                if let e = model.error { Text(e).font(Theme.caption).foregroundStyle(Theme.warn) }
                // Several cards scroll inside a fixed height, like the answer text: three tall
                // confirmation cards used to push the island's top (and its close button) off
                // the screen (2026-10-14).
                ScrollView(showsIndicators: model.cards.count > 1) {
                    VStack(spacing: 8) {
                        ForEach(model.cards) { card in
                            CardView(card: card, onConfirm: onConfirm, onOpen: onOpen, onFix: onFix)
                                .transition(.asymmetric(insertion: .move(edge: .top).combined(with: .opacity), removal: .opacity))
                        }
                    }
                }
                .frame(maxHeight: model.cards.count > 1 ? 240 : .infinity)
                .fixedSize(horizontal: false, vertical: model.cards.count <= 1)
                if let chip = model.taskChip { Chip(icon: "hourglass", text: chip) }
            }
            .padding(.horizontal, 16).padding(.bottom, 14)
            .background(GeometryReader { g in Color.clear.preference(key: HeightKey.self, value: g.size.height + 20) })
            .onPreferenceChange(HeightKey.self) { h in MainActor.assumeIsolated { model.panelHeight = h } }
        }
    }

    private var statusLine: String {
        if model.voiceOff, [.listening, .peek].contains(model.state) { return "Voice is off — results drop down as notifications" }
        switch model.state {
        case .listening: return model.pttHeld ? "Listening… release fn to send" : (model.handsFree ? (model.followUp ? "Anything else? (closes if you're done)" : "Listening (hands-free)…") : "Listening…")
        case .thinking: return "Thinking…"
        case .speaking: return model.characterName
        case .result: return model.hasConfirm ? "Needs your OK" : "Done"
        default: return ""
        }
    }
}

/// Sam's voice: three rings of light that turn and breathe like liquid, and swell with the
/// voice level (after the "AI voice animation" rings idea, in Sam's own colours). Listening is
/// green, thinking turns fast in dawn colours, speaking swells with Sam's own audio.
struct Orb: View {
    @ObservedObject var model: IslandModel
    var size: CGFloat
    @Environment(\.accessibilityReduceMotion) private var reduce
    /// One animator for every orb on screen: the peek orb and the open island's orb hand over
    /// mid-motion instead of restarting (two views at the same instant advance it once: dt = 0).
    private var anim: RingAnimator { RingAnimator.shared }
    var body: some View {
        // The rings and their glow draw on a canvas twice the orb's size, centred on it, so a swell
        // never hits the edge of the box; the orb still takes only `size` in the layout.
        TimelineView(.animation(minimumInterval: 1.0 / 120, paused: reduce)) { tl in
            let f = anim.step(mood: RingMood(model.state), level: reduce ? 0 : CGFloat(model.level), t: tl.date.timeIntervalSinceReferenceDate, still: reduce)
            VoiceRings(frame: f, size: size)
                .frame(width: size * 2, height: size * 2)
        }
        .frame(width: size, height: size)
        .allowsHitTesting(false)
        .accessibilityLabel("Sam is \(model.state.rawValue)")
    }
}

enum RingMood {
    case idle, listening, thinking, speaking
    init(_ s: IslandState) {
        switch s {
        case .listening, .dictating: self = .listening
        case .thinking: self = .thinking
        case .speaking: self = .speaking
        default: self = .idle
        }
    }
}

/// One drawn frame of the rings: everything already eased.
struct RingFrame {
    var colors: [SIMD3<Double>]
    var phases: [Double]      // rotation of each ring (integrated, never jumps)
    var shape: [Double]       // the wobble clock of each ring
    var wobble: CGFloat
    var swell: CGFloat
}

/// Sam's voice in motion. A state change never swaps the look: colours, speed, wobble and pulse
/// glide to their new values (≈ 0.7 s), rotation is integrated from the current speed (a faster
/// state speeds the rings up instead of jumping their angle), and the voice level is eased:
/// quick to rise, slow to fall, so the rings follow speech without twitching on every frame.
final class RingAnimator {
    static let shared = RingAnimator()
    /// At full voice level the rings grow by `grow` and their wobble rises by `shake`: the user's
    /// voice moves them more than Sam's own.
    struct Target { var colors: [SIMD3<Double>]; var speed: Double; var wobble: Double; var pulse: Double; var grow = 0.1; var shake = 0.45 }
    static let indigo = SIMD3(0.486, 0.549, 1.0), violet = SIMD3(0.78, 0.49, 1.0), white = SIMD3(1.0, 1.0, 1.0)
    static let green = SIMD3(0.298, 0.851, 0.392), cyan = SIMD3(0.39, 0.82, 1.0), peach = SIMD3(1.0, 0.60, 0.42)
    static func target(_ m: RingMood) -> Target {
        switch m {
        case .idle: return Target(colors: [indigo, violet, white], speed: 0.45, wobble: 0.7, pulse: 0)
        case .listening: return Target(colors: [green, cyan, white], speed: 0.8, wobble: 1.0, pulse: 0, grow: 0.2, shake: 0.7)
        case .thinking: return Target(colors: [indigo, violet, peach], speed: 1.9, wobble: 1.15, pulse: 0.035)
        case .speaking: return Target(colors: [indigo, peach, white], speed: 0.75, wobble: 1.0, pulse: 0)
        }
    }

    private var cur = RingAnimator.target(.idle)
    private var level: Double = 0
    private var phases: [Double] = [0, 2.1, 4.2]
    private var shape: [Double] = [0, 0, 0]
    private var pulsePhase: Double = 0
    private var last: Double = 0
    private let tempo: [Double] = [1.0, 1.27, 0.83]

    func step(mood: RingMood, level target: CGFloat, t: Double, still: Bool) -> RingFrame {
        let dt = last == 0 ? 1.0 / 60 : min(0.1, max(0, t - last))
        last = t
        let goal = Self.target(mood)
        if still { cur = goal }
        let k = 1 - exp(-dt / 0.55)            // parameter glide
        for i in 0..<3 { cur.colors[i] += (goal.colors[i] - cur.colors[i]) * k }
        cur.speed += (goal.speed - cur.speed) * k
        cur.wobble += (goal.wobble - cur.wobble) * k
        cur.pulse += (goal.pulse - cur.pulse) * k
        cur.grow += (goal.grow - cur.grow) * k
        cur.shake += (goal.shake - cur.shake) * k
        let l = Double(min(max(target, 0), 1))
        level += (l - level) * (1 - exp(-dt / (l > level ? 0.09 : 0.35)))
        if !still {
            for i in 0..<3 {
                phases[i] += dt * cur.speed * tempo[i] * (i == 1 ? -1 : 1)
                shape[i] += dt * tempo[i]
            }
            pulsePhase += dt * 2.6
        }
        let swell = 1 + level * cur.grow + cur.pulse * sin(pulsePhase)
        return RingFrame(colors: cur.colors, phases: phases, shape: shape, wobble: CGFloat(cur.wobble * (1 + level * cur.shake)), swell: CGFloat(swell))
    }
}

struct VoiceRings: View {
    var frame: RingFrame
    /// The orb's own size; the canvas around it is larger, room for the glow.
    var size: CGFloat

    var body: some View {
        Canvas { ctx, sz in
            let c = CGPoint(x: sz.width / 2, y: sz.height / 2)
            let base = size / 2 * 0.74
            for i in 0..<3 {
                let rgb = frame.colors[i]
                let color = Color(red: rgb.x, green: rgb.y, blue: rgb.z)
                let r = base * frame.swell * (1 - CGFloat(i) * 0.03)
                let path = Self.blob(center: c, radius: r, rotation: frame.phases[i], t: frame.shape[i], index: i, wobble: frame.wobble)
                // The third ring is the thin bright one (white, or peach while thinking).
                let thin = i == 2
                ctx.drawLayer { l in
                    l.addFilter(.blur(radius: max(1, r * 0.2)))
                    l.stroke(path, with: .color(color.opacity(thin ? 0.35 : 0.6)), lineWidth: max(1.5, r * 0.3))
                }
                var bright = ctx
                bright.blendMode = .plusLighter
                bright.stroke(path, with: .color(thin ? color.opacity(0.7) : color), lineWidth: max(0.8, r * (thin ? 0.045 : 0.11)))
            }
        }
        .accessibilityHidden(true)
    }

    /// A closed, softly deformed circle (the pen's morphing border-radius, as a path).
    static func blob(center c: CGPoint, radius r: CGFloat, rotation: Double, t: Double, index i: Int, wobble: CGFloat) -> Path {
        var p = Path()
        let n = 96
        for k in 0...n {
            let th = Double(k) / Double(n) * 2 * .pi
            let d = 0.07 * sin(2 * th + t * 0.9 + Double(i) * 1.7) + 0.04 * sin(3 * th - t * 0.6 + Double(i) * 2.3)
            let rr = r * (1 + wobble * CGFloat(d))
            let a = th + rotation
            let pt = CGPoint(x: c.x + rr * CGFloat(cos(a)), y: c.y + rr * CGFloat(sin(a)))
            if k == 0 { p.move(to: pt) } else { p.addLine(to: pt) }
        }
        p.closeSubpath()
        return p
    }
}

struct Waveform: View {
    var bands: [Float]
    var color: Color
    var body: some View {
        HStack(alignment: .center, spacing: 2) {
            ForEach(Array(bands.enumerated()), id: \.offset) { _, b in
                Capsule().fill(color.opacity(0.85)).frame(width: 3, height: max(3, CGFloat(b) * 22))
            }
        }
        .animation(.linear(duration: 0.06), value: bands)
        .accessibilityHidden(true)
    }
}

struct Chip: View {
    var icon: String
    var text: String
    var body: some View {
        HStack(spacing: 4) { if !icon.isEmpty { Image(systemName: icon).font(Theme.micro) }; Text(text).lineLimit(1) }
            .font(Theme.caption).foregroundStyle(Theme.text2)
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(Capsule().fill(Theme.fill))
            .overlay(Capsule().strokeBorder(Theme.stroke))
    }
}

// MARK: - Cards

struct CardView: View {
    let card: Card
    var onConfirm: (String, String, [String: Any]) -> Void
    var onOpen: (String) -> Void
    var onFix: (Card) -> Void
    @State private var edits: [String: String] = [:]
    @State private var editing = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(card.blocks) { b in BlockView(block: b, openable: card.open != nil, onOpen: { if let f = card.open { onOpen(f) } }) }
            if card.kind == "confirm", let c = card.confirm { confirmBody(c) }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .samCard(tone: card.kind == "error" ? Theme.warn : nil)
    }

    @ViewBuilder private func confirmBody(_ c: [String: Any]) -> some View {
        let id = c["action_id"] as? String ?? ""
        let fields = (c["fields"] as? [[String: Any]]) ?? []
        VStack(alignment: .leading, spacing: 6) {
            // Empty fields only appear while editing.
            ForEach(Array(fields.filter { editing || !((($0["value"] as? String) ?? "").isEmpty) }.enumerated()), id: \.offset) { _, f in
                let key = f["key"] as? String ?? ""
                let label = f["label"] as? String ?? key
                let value = f["value"] as? String ?? ""
                HStack(alignment: .top, spacing: 8) {
                    Text(label).font(Theme.caption).foregroundStyle(Theme.text3).frame(width: 58, alignment: .leading)
                    if editing {
                        TextField(label, text: Binding(get: { edits[key] ?? value }, set: { edits[key] = $0 }), axis: .vertical)
                            .textFieldStyle(.plain).font(Theme.body).foregroundStyle(Theme.text).lineLimit(1...4)
                            .accessibilityLabel("Edit \(label)")
                    } else {
                        Text(edits[key] ?? (f["display"] as? String) ?? value).font(Theme.body).foregroundStyle(Theme.text).lineLimit((f["multiline"] as? Bool) == true ? 4 : 1)
                    }
                }
            }
            HStack(spacing: 8) {
                // No keyboard shortcut: an approval must be a deliberate click (or "yes, go ahead"),
                // never a Return typed into another app (lab: that approved a calendar event).
                Button("Approve") { onConfirm(id, "approve", editedArgs(fields)) }
                    .buttonStyle(IslandButton(primary: true))
                Button(editing ? "Done" : "Edit") { editing.toggle() }.buttonStyle(IslandButton(primary: false))
                Spacer()
                Button("Cancel") { onConfirm(id, "cancel", [:]) }.buttonStyle(IslandButton(primary: false, destructive: true)).keyboardShortcut(.cancelAction)
            }
            .padding(.top, 2)
        }
    }

    private func editedArgs(_ fields: [[String: Any]]) -> [String: Any] {
        var out: [String: Any] = [:]
        for f in fields {
            guard let k = f["key"] as? String, let v = edits[k], v != (f["value"] as? String) else { continue }
            out[k] = (f["array"] as? Bool) == true ? v.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) } : v
        }
        return out
    }
}

struct IslandButton: ButtonStyle {
    var primary: Bool
    var destructive = false
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.font(Theme.caption.weight(.semibold))
            .foregroundStyle(primary ? .white : (destructive ? Theme.bad : Theme.text))
            .padding(.horizontal, 12).padding(.vertical, 6)
            .background(Capsule().fill(primary ? Theme.accent : (configuration.isPressed ? Theme.fillSelected : Theme.fillHover)))
            .overlay(Capsule().strokeBorder(primary ? Color.white.opacity(0.18) : Theme.stroke))
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
    }
}

struct BlockView: View {
    let block: CardBlock
    var openable: Bool
    var onOpen: () -> Void
    var body: some View {
        let b = block.raw
        if let v = b["view"] as? [String: Any] {
            // A typed result (now playing, weather, an added reminder or event).
            ZStack(alignment: .topTrailing) {
                TypedResultView(view: v)
                if openable && !["music", "files"].contains(v["type"] as? String ?? "") {
                    Button(action: onOpen) { Image(systemName: "arrow.up.forward").font(.system(size: 9, weight: .bold)).frame(width: 20, height: 20) }
                        .buttonStyle(.plain).foregroundStyle(Theme.text2).background(Circle().fill(Theme.fillHover)).accessibilityLabel("Open")
                        .offset(x: 4, y: -4)
                }
            }
        } else if let h = b["header"] as? [String: Any] {
            HStack(spacing: 10) {
                IconTile(symbol: h["icon"] as? String ?? "sparkles", size: 28)
                Text(h["title"] as? String ?? "").font(Theme.title).foregroundStyle(Theme.text).lineLimit(1)
                Spacer(minLength: 6)
                if let d = h["detail"] as? String, !d.isEmpty {
                    // "done" / "failed" read as status pills; anything else as quiet detail text.
                    switch d {
                    case "done": Pill(text: "Done", color: Theme.good)
                    case "failed": Pill(text: "Failed", color: Theme.bad)
                    default: Text(d).font(Theme.caption).foregroundStyle(Theme.text3).lineLimit(1)
                    }
                }
                if openable {
                    Button(action: onOpen) { Image(systemName: "arrow.up.forward").font(.system(size: 10, weight: .bold)).frame(width: 22, height: 22) }
                        .buttonStyle(.plain).foregroundStyle(Theme.text2).background(Circle().fill(Theme.fillHover)).accessibilityLabel("Open")
                }
            }
        } else if let w = b["warning"] as? String {
            HStack(alignment: .top, spacing: 6) {
                Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(Theme.warn)
                Text(w).font(Theme.caption).foregroundStyle(Theme.warn).lineLimit(2)
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
            .background(RoundedRectangle(cornerRadius: Theme.chipRadius).fill(Theme.warn.opacity(0.10)))
        } else if let t = b["text"] as? String {
            Text(.init(t)).font(Theme.body).foregroundStyle(Theme.text2).lineSpacing(2).lineLimit(6).fixedSize(horizontal: false, vertical: true)
        } else if let rows = b["list"] as? [[String: Any]] {
            VStack(spacing: 6) {
                ForEach(Array(rows.prefix(6).enumerated()), id: \.offset) { _, r in
                    HStack {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(r["title"] as? String ?? "").font(Theme.body).foregroundStyle(Theme.text).lineLimit(1)
                            if let s = r["subtitle"] as? String { Text(s).font(Theme.caption).foregroundStyle(Theme.text2).lineLimit(1) }
                        }
                        Spacer()
                        if let tr = r["trailing"] as? String { Text(tr).font(Theme.caption).foregroundStyle(Theme.text2) }
                    }
                }
            }
        } else if let stats = b["stats"] as? [[String: Any]] {
            HStack(spacing: 18) {
                ForEach(Array(stats.prefix(3).enumerated()), id: \.offset) { _, s in
                    VStack(alignment: .leading, spacing: 0) {
                        Text(s["value"] as? String ?? "").font(Theme.stat).foregroundStyle(s["tone"] == nil ? Theme.text : Theme.tone(s["tone"] as? String))
                        Text(s["label"] as? String ?? "").font(Theme.caption).foregroundStyle(Theme.text3)
                    }
                }
            }
        } else if let kv = b["keyValue"] as? [[String: Any]] {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(kv.prefix(5).enumerated()), id: \.offset) { _, p in
                    HStack { Text(p["label"] as? String ?? "").font(Theme.caption).foregroundStyle(Theme.text3).frame(width: 80, alignment: .leading); Text(p["value"] as? String ?? "").font(Theme.body).foregroundStyle(Theme.text).lineLimit(1) }
                }
            }
        } else if let badges = b["badges"] as? [[String: Any]] {
            HStack(spacing: 6) { ForEach(Array(badges.prefix(3).enumerated()), id: \.offset) { _, x in Chip(icon: "", text: x["label"] as? String ?? "") } }
        } else if let p = b["progress"] as? [String: Any] {
            ProgressView(value: (p["value"] as? Double ?? 0) / 100).tint(Theme.accent)
        } else {
            EmptyView()
        }
    }
}
