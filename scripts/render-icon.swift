// Renders an SVG to a 1024-point PNG with a transparent background, for the app icon.
// QuickLook's thumbnails (`qlmanage -t`) fill the background white, which shows round the tile.
//
//   swift scripts/render-icon.swift assets/icon.svg out.png
import AppKit
let a = CommandLine.arguments
guard let svg = NSImage(contentsOfFile: a[1]) else { FileHandle.standardError.write("can't read \(a[1])\n".data(using: .utf8)!); exit(1) }
let n = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: n, pixelsHigh: n, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
svg.draw(in: NSRect(x: 0, y: 0, width: n, height: n))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[2]))
