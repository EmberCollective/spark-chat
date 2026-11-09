// Preserve the original artwork; AppKit supplies the rounded-rectangle clipping.
import AppKit

guard CommandLine.arguments.count == 3,
      let source = NSImage(contentsOfFile: CommandLine.arguments[1]),
      let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 1024, pixelsHigh: 1024,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
      let context = NSGraphicsContext(bitmapImageRep: bitmap) else { fatalError("Invalid icon input") }
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
NSColor.clear.setFill()
NSRect(x: 0, y: 0, width: 1024, height: 1024).fill()
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
NSBezierPath(roundedRect: tile, xRadius: 184, yRadius: 184).addClip()
source.draw(in: tile, from: .zero, operation: .copy, fraction: 1)
NSGraphicsContext.restoreGraphicsState()
guard let png = bitmap.representation(using: .png, properties: [:]) else { fatalError("PNG export failed") }
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[2]), options: .atomic)
