#!/usr/bin/env python3
"""Generate AIVPN app icon using macOS system tools"""
import subprocess, os, shutil

# Create a simple colored icon using CoreImage via swift
swift_code = '''
import Cocoa
import AppKit

let size = 256
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

// Shield shape using bezier path
let path = NSBezierPath()
path.move(to: NSPoint(x: 128, y: 228))
path.curve(to: NSPoint(x: 228, y: 180), controlPoint1: NSPoint(x: 200, y: 228), controlPoint2: NSPoint(x: 228, y: 210))
path.curve(to: NSPoint(x: 228, y: 100), controlPoint1: NSPoint(x: 228, y: 140), controlPoint2: NSPoint(x: 228, y: 120))
path.curve(to: NSPoint(x: 128, y: 28), controlPoint1: NSPoint(x: 228, y: 60), controlPoint2: NSPoint(x: 180, y: 28))
path.curve(to: NSPoint(x: 28, y: 100), controlPoint1: NSPoint(x: 76, y: 28), controlPoint2: NSPoint(x: 28, y: 60))
path.curve(to: NSPoint(x: 28, y: 180), controlPoint1: NSPoint(x: 28, y: 120), controlPoint2: NSPoint(x: 28, y: 140))
path.curve(to: NSPoint(x: 128, y: 228), controlPoint1: NSPoint(x: 28, y: 210), controlPoint2: NSPoint(x: 56, y: 228))
path.close()

// Fill with gradient
let gradient = NSGradient(colors: [
    NSColor(calibratedRed: 0.18, green: 0.49, blue: 0.27, alpha: 1.0),
    NSColor(calibratedRed: 0.11, green: 0.37, blue: 0.13, alpha: 1.0)
])
gradient?.draw(in: path.bounds, angle: 90)

// Stroke
path.lineWidth = 3
NSColor(calibratedRed: 0.08, green: 0.29, blue: 0.10, alpha: 1.0).setStroke()
path.stroke()

// Checkmark
let check = NSBezierPath()
check.move(to: NSPoint(x: 80, y: 128))
check.line(to: NSPoint(x: 115, y: 95))
check.line(to: NSPoint(x: 178, y: 160))
check.lineWidth = 8
check.lineCapStyle = .round
check.lineJoinStyle = .round
NSColor.white.setStroke()
check.stroke()

image.unlockFocus()

// Save as PNG
guard let tiffData = image.tiffRepresentation,
      let bitmap = NSBitmapImageRep(data: tiffData),
      let pngData = bitmap.representation(using: .png, properties: [:]) else {
    print("Failed to create PNG")
    exit(1)
}

let pngPath = "/tmp/aivpn_icon.png"
try! pngData.write(to: URL(fileURLWithPath: pngPath))
print("Created \\(pngPath)")

// Create iconset
let iconset = "/tmp/Aivpn.iconset"
try? FileManager.default.removeItem(atPath: iconset)
try FileManager.default.createDirectory(atPath: iconset, withIntermediateDirectories: true)

let sizes: [(Int, String)] = [
    (16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png"),
]

for (s, name) in sizes {
    let out = "\(iconset)/\(name)"
    let rep = NSBitmapImageRep(data: tiffData)!
    let newImage = NSImage(size: NSSize(width: s, height: s))
    newImage.lockFocus()
    NSGraphicsContext.current?.imageInterpolation = .high
    rep.draw(in: NSRect(x: 0, y: 0, width: s, height: s))
    newImage.unlockFocus()
    
    guard let newTiff = newImage.tiffRepresentation,
          let newBitmap = NSBitmapImageRep(data: newTiff),
          let newData = newBitmap.representation(using: .png, properties: [:]) else { continue }
    try! newData.write(to: URL(fileURLWithPath: out))
}

// Convert to icns
let process = Process()
process.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
process.arguments = ["-c", "icns", iconset, "-o", "/tmp/Aivpn.icns"]
try! process.run()
process.waitUntilExit()

if FileManager.default.fileExists(atPath: "/tmp/Aivpn.icns") {
    let attrs = try! FileManager.default.attributesOfItem(atPath: "/tmp/Aivpn.icns")
    let size = attrs[.size] as! Int64
    print("Created /tmp/Aivpn.icns (\\(size) bytes)")
} else {
    print("Failed to create icns")
}
'''

result = subprocess.run(['swift', '-e', swift_code], capture_output=True, text=True)
print(result.stdout)
if result.stderr:
    print("STDERR:", result.stderr[:500])
