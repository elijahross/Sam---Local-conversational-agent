// Design tokens (app/docs/design-system.md §2).
import SwiftUI

enum Theme {
    static let island = Color.black
    static let card = Color(red: 0.078, green: 0.078, blue: 0.086)
    static let cardHover = Color(red: 0.11, green: 0.11, blue: 0.122)
    static let stroke = Color.white.opacity(0.08)
    static let strokeStrong = Color.white.opacity(0.14)
    /// One window background for every surface (onboarding, connectors, overview): see AppBackdrop.
    static let bgDeep = Color(red: 0.04, green: 0.045, blue: 0.07)
    /// Fills on glass, three steps: at rest, hovered/pressed, selected.
    static let fill = Color.white.opacity(0.065)
    static let fillHover = Color.white.opacity(0.10)
    static let fillSelected = Color.white.opacity(0.14)
    /// Glass tints: panes and cards on the backdrop; the island and the menu-bar dropdown sit on any wallpaper.
    static let glassTint = Color.black.opacity(0.30)
    static let islandTint = Color.black.opacity(0.55)
    static let text = Color(red: 0.96, green: 0.96, blue: 0.97)
    static let text2 = Color(red: 0.63, green: 0.63, blue: 0.65)
    static let text3 = Color(red: 0.43, green: 0.43, blue: 0.45)
    static let accent = Color(red: 0.486, green: 0.549, blue: 1.0)
    static let listening = Color(red: 0.298, green: 0.851, blue: 0.392)
    static let thinkA = Color(red: 0.486, green: 0.549, blue: 1.0)
    static let thinkB = Color(red: 0.78, green: 0.49, blue: 1.0)
    static let good = Color(red: 0.19, green: 0.82, blue: 0.35)
    static let bad = Color(red: 1.0, green: 0.27, blue: 0.23)
    static let warn = Color(red: 1.0, green: 0.84, blue: 0.04)

    /// Type scale (SF Pro): display for the welcome screen, h1 for page titles, h2 for pane and
    /// detail titles, h3 for section headings, title for card headers, lead for intros, body, caption,
    /// label for small bold labels, overline for uppercase section names, micro for badge glyphs.
    static let display = Font.system(size: 30, weight: .bold, design: .rounded)
    static let h1 = Font.system(size: 26, weight: .bold)
    static let h2 = Font.system(size: 20, weight: .semibold)
    static let h3 = Font.system(size: 17, weight: .semibold)
    static let title = Font.system(size: 15, weight: .semibold)
    static let lead = Font.system(size: 14)
    static let body = Font.system(size: 13)
    static let caption = Font.system(size: 11, weight: .medium)
    static let label = Font.system(size: 12, weight: .semibold)
    static let overline = Font.system(size: 10, weight: .semibold)
    static let micro = Font.system(size: 9, weight: .semibold)
    /// Soft accent wash behind icons and tags.
    static let accentSoft = Color(red: 0.486, green: 0.549, blue: 1.0).opacity(0.16)
    static let stat = Font.system(size: 24, weight: .semibold, design: .rounded).monospacedDigit()
    static let mono = Font.system(size: 11, design: .monospaced)

    static let panelWidth: CGFloat = 420
    static let panelMaxHeight: CGFloat = 420
    static let cardRadius: CGFloat = 14
    static let tileRadius: CGFloat = 12
    static let chipRadius: CGFloat = 8
    static let buttonRadius: CGFloat = 9
    static let paneRadius: CGFloat = 24
    static let panelRadius: CGFloat = 22

    static func shape(_ reduce: Bool) -> Animation { reduce ? .easeInOut(duration: 0.15) : .spring(response: 0.35, dampingFraction: 0.82) }
    static func content(_ reduce: Bool) -> Animation { reduce ? .easeInOut(duration: 0.15) : .smooth(duration: 0.25) }
    static func tone(_ t: String?) -> Color {
        switch t { case "good": return good; case "bad": return bad; case "warn": return warn; default: return text2 }
    }
}
