// Sam's logo: the mark from media/logo-black.svg / logo-white.svg (copied into the
// bundle's Resources by build.sh), drawn as a vector path so it is crisp at every size and takes
// the theme's colour: a template image in the menu bar (macOS tints it for light and dark menu
// bars), the theme's text colour in the app. tools/make_icon.swift renders the app and dmg icons
// from the same path.
import AppKit
import SwiftUI

enum SamLogo {
    /// The SVG's 24×24 viewBox.
    static let viewBox = CGSize(width: 24, height: 24)

    /// Path data of the logo: from the bundled SVG when there is one, else the copy below (same
    /// file, so a build without Resources still shows the mark).
    static let pathData: [String] = {
        let urls = [Bundle.main.url(forResource: "logo-black", withExtension: "svg"), ProcessInfo.processInfo.environment["SAM_LOGO_SVG"].map { URL(fileURLWithPath: $0) }]
        for case let u? in urls {
            if let s = try? String(contentsOf: u, encoding: .utf8) {
                let d = parseD(s)
                if !d.isEmpty { return d }
            }
        }
        return fallback
    }()

    /// `d="…"` attributes of the SVG's paths.
    static func parseD(_ svg: String) -> [String] {
        guard let re = try? NSRegularExpression(pattern: #"<path[^>]*\sd="([^"]+)""#) else { return [] }
        let ns = svg as NSString
        return re.matches(in: svg, range: NSRange(location: 0, length: ns.length)).map { ns.substring(with: $0.range(at: 1)) }
    }

    /// The whole mark in viewBox units (y down, as in the SVG).
    static let cgPath: CGPath = {
        let p = CGMutablePath()
        for d in pathData { add(d, to: p) }
        return p
    }()

    /// The mark scaled into `rect`, keeping its proportions; `flipY` for y-up contexts (AppKit
    /// drawing without a flipped image).
    static func path(in rect: CGRect, flipY: Bool = false) -> CGPath {
        let s = min(rect.width / viewBox.width, rect.height / viewBox.height)
        var t = CGAffineTransform(translationX: rect.midX - viewBox.width * s / 2, y: rect.midY - viewBox.height * s / 2).scaledBy(x: s, y: s)
        if flipY { t = CGAffineTransform(translationX: 0, y: viewBox.height).scaledBy(x: 1, y: -1).concatenating(t) }
        return cgPath.copy(using: &t) ?? cgPath
    }

    /// A template image for the menu bar: macOS draws it dark or light to match the bar.
    static func templateImage(pointSize: CGFloat) -> NSImage {
        let img = NSImage(size: NSSize(width: pointSize, height: pointSize), flipped: true) { r in
            guard let ctx = NSGraphicsContext.current?.cgContext else { return false }
            ctx.addPath(path(in: r))
            ctx.setFillColor(NSColor.black.cgColor)
            ctx.fillPath()
            return true
        }
        img.isTemplate = true
        img.accessibilityDescription = "Sam"
        return img
    }

    // MARK: SVG path data (absolute and relative M L H V C S Q T Z)

    static func add(_ d: String, to p: CGMutablePath) {
        var nums: [CGFloat] = []
        var cmd: Character = " "
        var cur = CGPoint.zero, start = CGPoint.zero, lastCtrl: CGPoint?
        func flush() {
            let rel = cmd.isLowercase
            var i = 0
            func pt() -> CGPoint {
                let q = CGPoint(x: nums[i], y: nums[i + 1]); i += 2
                return rel ? CGPoint(x: cur.x + q.x, y: cur.y + q.y) : q
            }
            repeat {
                switch cmd.uppercased() {
                case "M":
                    guard i + 1 < nums.count else { return }
                    cur = pt(); start = cur; p.move(to: cur); lastCtrl = nil
                    cmd = rel ? "l" : "L" // further pairs are lines
                case "L":
                    guard i + 1 < nums.count else { return }
                    cur = pt(); p.addLine(to: cur); lastCtrl = nil
                case "H":
                    guard i < nums.count else { return }
                    cur.x = rel ? cur.x + nums[i] : nums[i]; i += 1; p.addLine(to: cur); lastCtrl = nil
                case "V":
                    guard i < nums.count else { return }
                    cur.y = rel ? cur.y + nums[i] : nums[i]; i += 1; p.addLine(to: cur); lastCtrl = nil
                case "C":
                    guard i + 5 < nums.count else { return }
                    let c1 = pt(), c2 = pt(), e = pt()
                    p.addCurve(to: e, control1: c1, control2: c2); cur = e; lastCtrl = c2
                case "S":
                    guard i + 3 < nums.count else { return }
                    let c1 = lastCtrl.map { CGPoint(x: 2 * cur.x - $0.x, y: 2 * cur.y - $0.y) } ?? cur
                    let c2 = pt(), e = pt()
                    p.addCurve(to: e, control1: c1, control2: c2); cur = e; lastCtrl = c2
                case "Q":
                    guard i + 3 < nums.count else { return }
                    let c = pt(), e = pt()
                    p.addQuadCurve(to: e, control: c); cur = e; lastCtrl = c
                case "T":
                    guard i + 1 < nums.count else { return }
                    let c = lastCtrl.map { CGPoint(x: 2 * cur.x - $0.x, y: 2 * cur.y - $0.y) } ?? cur
                    let e = pt()
                    p.addQuadCurve(to: e, control: c); cur = e; lastCtrl = c
                case "Z":
                    p.closeSubpath(); cur = start; lastCtrl = nil
                    return
                default:
                    return
                }
            } while i < nums.count
        }
        var token = ""
        func endNumber() { if let v = Double(token) { nums.append(CGFloat(v)) }; token = "" }
        for ch in d {
            if ch.isLetter && ch != "e" && ch != "E" {
                endNumber(); flush(); nums = []; cmd = ch
                if ch == "Z" || ch == "z" { flush(); cmd = " " }
            } else if ch == "-" && !token.isEmpty && !token.hasSuffix("e") && !token.hasSuffix("E") {
                endNumber(); token = "-"
            } else if ch == "." && token.contains(".") {
                endNumber(); token = "."
            } else if ch == "," || ch == " " || ch == "\n" || ch == "\t" {
                endNumber()
            } else {
                token.append(ch)
            }
        }
        endNumber(); flush()
    }

    /// logo-black.svg as of 2026-10-15.
    static let fallback = [
        "M11.2499 3.90059V12.9206C11.2499 13.2506 11.0899 13.5506 10.8199 13.7406L3.60993 18.7906C3.01993 19.2006 2.17993 18.8806 2.04993 18.1706C1.89993 17.3106 2.09993 16.3006 2.69993 15.2106L5.81993 9.60059L8.75993 4.31059C8.98993 3.90059 9.23993 3.54059 9.49993 3.23059C10.0999 2.53059 11.2499 2.98059 11.2499 3.90059Z",
        "M20.39 18.7906L13.18 13.7406C12.91 13.5506 12.75 13.2506 12.75 12.9206V3.90059C12.75 2.98059 13.9 2.53059 14.5 3.23059C14.76 3.54059 15.01 3.90059 15.24 4.31059L18.18 9.60059L21.3 15.2106C21.9 16.3006 22.1 17.3106 21.95 18.1706C21.82 18.8806 20.98 19.2006 20.39 18.7906Z",
        "M5.25994 19.14L11.4099 15.04C11.7399 14.82 12.2799 14.82 12.6099 15.04L18.7599 19.14C20.3899 20.23 20.1199 21.12 18.1599 21.12H5.84994C3.89994 21.11 3.62994 20.22 5.25994 19.14Z",
    ]
}

/// The mark as a SwiftUI shape (fill it with any colour).
struct SamLogoShape: Shape {
    func path(in rect: CGRect) -> Path { Path(SamLogo.path(in: rect)) }
}

/// The mark in the theme's text colour by default (white on Sam's dark glass, black on light).
struct SamLogoView: View {
    var size: CGFloat = 18
    var color: Color = .primary
    var body: some View {
        SamLogoShape().fill(color).frame(width: size, height: size).accessibilityLabel("Sam")
    }
}
