// Renders Sam's app icon (and the dmg's volume icon) from the root logo SVG:
//   swiftc tools/make_icon/main.swift Sources/Logo.swift -o build/make_icon && build/make_icon <out.iconset> [logo.svg]
//   build/make_icon --set-icon <icns> <file>   (the dmg file's own Finder icon)
// A dark rounded tile (Sam's glass: deep ink to indigo) with the white mark, macOS icon grid:
// 824-pt tile on a 1024 canvas, the mark at ~52% of the tile.
import AppKit

let args = CommandLine.arguments
// make_icon --set-icon <icns> <file>: give a file (the .dmg) a custom Finder icon.
if args.count == 4 && args[1] == "--set-icon" {
    guard let img = NSImage(contentsOfFile: args[2]) else { print("can't read \(args[2])"); exit(1) }
    exit(NSWorkspace.shared.setIcon(img, forFile: args[3], options: []) ? 0 : 1)
}
guard args.count >= 2 else { print("usage: make_icon <out.iconset> [logo.svg]"); exit(2) }
if args.count >= 3 { setenv("SAM_LOGO_SVG", args[2], 1) }
let out = URL(fileURLWithPath: args[1])
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

func render(_ px: Int) -> Data {
    let cs = CGColorSpace(name: CGColorSpace.displayP3)!
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let s = CGFloat(px) / 1024
    ctx.scaleBy(x: s, y: s)
    // Tile with a soft drop shadow (the macOS icon grid leaves room for it).
    let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
    let shape = CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 24, color: CGColor(gray: 0, alpha: 0.35))
    ctx.addPath(shape); ctx.setFillColor(CGColor(srgbRed: 0.07, green: 0.07, blue: 0.10, alpha: 1)); ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(shape); ctx.clip()
    let grad = CGGradient(colorsSpace: cs, colors: [CGColor(srgbRed: 0.20, green: 0.19, blue: 0.33, alpha: 1), CGColor(srgbRed: 0.07, green: 0.07, blue: 0.11, alpha: 1)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(grad, start: CGPoint(x: 200, y: 924), end: CGPoint(x: 824, y: 100), options: [])
    // A faint inner highlight along the top edge.
    ctx.addPath(shape); ctx.setStrokeColor(CGColor(gray: 1, alpha: 0.10)); ctx.setLineWidth(4); ctx.strokePath()
    ctx.restoreGState()
    // The mark, white, centred (y-up context: flip the SVG path).
    let m: CGFloat = 430
    let mark = SamLogo.path(in: CGRect(x: 512 - m / 2, y: 512 - m / 2 - 6, width: m, height: m), flipY: true)
    ctx.addPath(mark); ctx.setFillColor(CGColor(srgbRed: 1, green: 0.99, blue: 0.99, alpha: 1)); ctx.fillPath()
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    return rep.representation(using: .png, properties: [:])!
}

for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128), ("128x128@2x", 256), ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    try! render(px).write(to: out.appendingPathComponent("icon_\(name).png"))
}
print("wrote \(out.path)")
