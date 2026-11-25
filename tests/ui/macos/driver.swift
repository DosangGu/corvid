import AppKit
import ApplicationServices
import CoreGraphics
import Foundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

let arguments = CommandLine.arguments
let command = arguments[1]

if command == "frontmost" {
    if let app = NSWorkspace.shared.frontmostApplication {
        print("\(app.processIdentifier) \(app.bundleIdentifier ?? "-")")
    }
    exit(0)
}

let pid = Int(arguments[2])!

func visibleWindows() -> [[String: Any]] {
    CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID)
        as? [[String: Any]] ?? []
}

if command == "window" {
    for window in visibleWindows() where window[kCGWindowOwnerPID as String] as? Int == pid {
        let layer = window[kCGWindowLayer as String] as? Int
        if layer == 0 || layer == NSWindow.Level.popUpMenu.rawValue {
            print(window[kCGWindowNumber as String] as! Int)
            break
        }
    }
    exit(0)
}

guard CGPreflightPostEventAccess() else {
    fail("macOS denied synthetic input. Enable Accessibility for the test launcher.")
}

if command == "click" || command == "size" {
    let number = Int(arguments[3])!
    guard let window = visibleWindows().first(where: {
        $0[kCGWindowOwnerPID as String] as? Int == pid
            && $0[kCGWindowNumber as String] as? Int == number
    }), let bounds = window[kCGWindowBounds as String] as? [String: Any],
       let rect = CGRect(dictionaryRepresentation: bounds as CFDictionary) else {
        fail("Fixture window \(number) is no longer visible")
    }
    if command == "size" {
        print("\(Int(rect.width)) \(Int(rect.height))")
        exit(0)
    }
    let point = CGPoint(x: rect.minX + Double(arguments[4])!, y: rect.minY + Double(arguments[5])!)
    guard rect.contains(point) else { fail("Click falls outside fixture window \(number)") }
    for type in [CGEventType.leftMouseDown, .leftMouseUp] {
        guard let event = CGEvent(mouseEventSource: nil, mouseType: type,
                                  mouseCursorPosition: point, mouseButton: .left) else {
            fail("Could not create macOS mouse event")
        }
        event.setIntegerValueField(.mouseEventClickState, value: 1)
        event.post(tap: .cghidEventTap)
    }
    exit(0)
}

if command == "minimize" {
    let app = AXUIElementCreateApplication(pid_t(pid))
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
          let windows = value as? [AXUIElement], windows.count == 1,
          AXUIElementSetAttributeValue(windows[0], kAXMinimizedAttribute as CFString,
                                       kCFBooleanTrue) == .success else {
        fail("Could not minimize the single Desktop fixture window")
    }
    exit(0)
}

guard let app = NSRunningApplication(processIdentifier: pid_t(pid)) else {
    fail("Fixture application is no longer available")
}
if command != "keys-focused" && !app.isActive {
    app.activate(options: [])
    let deadline = Date().addingTimeInterval(2)
    while !app.isActive && Date() < deadline {
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
}

guard app.isActive else { fail("Fixture window lacks macOS keyboard focus") }

if command == "submit" {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    let text = String(decoding: data, as: UTF8.self)
    let clipboard = NSPasteboard.general
    clipboard.clearContents()
    guard clipboard.setString(text, forType: .string) else {
        fail("Could not write the fixture clipboard")
    }
}

let keys: [String: (CGKeyCode, CGEventFlags)] = [
    "leader": (30, .maskControl), "t": (17, []), "n": (45, []), "h": (4, []),
    "paste": (9, .maskCommand), "return": (36, []),
    "clear": (32, .maskControl),
    "0": (29, []), "1": (18, []), "2": (19, []), "3": (20, []),
    "4": (21, []), "5": (23, []), "6": (22, []), "7": (26, []),
    "8": (28, []), "9": (25, []),
]
for name in arguments.dropFirst(3) {
    guard app.isActive else { fail("Fixture window lost macOS keyboard focus") }
    if command == "keys-focused" && NSWorkspace.shared.frontmostApplication?.processIdentifier != app.processIdentifier {
        fail("Fixture application is no longer frontmost")
    }
    let (code, flags) = keys[name]!
    for down in [true, false] {
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down) else {
            fail("Could not create macOS keyboard event")
        }
        event.flags = flags
        event.post(tap: .cghidEventTap)
    }
    Thread.sleep(forTimeInterval: 0.1)
}
