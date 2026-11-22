import AppKit
import CoreGraphics
import Foundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

let arguments = CommandLine.arguments
let pid = Int(arguments[2])!

if arguments[1] == "window" {
    let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID)
        as? [[String: Any]] ?? []
    for window in windows where window[kCGWindowOwnerPID as String] as? Int == pid {
        let layer = window[kCGWindowLayer as String] as? Int
        if layer == 0 || layer == NSWindow.Level.popUpMenu.rawValue {
            print(window[kCGWindowNumber as String] as! Int)
            break
        }
    }
    exit(0)
}

guard CGPreflightPostEventAccess() else {
    fail("macOS denied synthetic keyboard events. Enable Accessibility for the test launcher.")
}
guard let app = NSRunningApplication(processIdentifier: pid_t(pid)) else {
    fail("Fixture application is no longer available")
}
if !app.isActive {
    app.activate(options: [])
    let deadline = Date().addingTimeInterval(2)
    while !app.isActive && Date() < deadline {
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
}

if arguments[1] == "submit" {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    let text = String(decoding: data, as: UTF8.self)
    let clipboard = NSPasteboard.general
    clipboard.clearContents()
    guard clipboard.setString(text, forType: .string) else {
        fail("Could not write the fixture clipboard")
    }
}

let keys: [String: (CGKeyCode, CGEventFlags)] = [
    "leader": (30, .maskControl), "t": (17, []), "n": (45, []),
    "previous": (33, []), "paste": (9, .maskCommand), "return": (36, []),
    "clear": (32, .maskControl),
]
for name in arguments.dropFirst(3) {
    guard app.isActive else { fail("Fixture window lost macOS keyboard focus") }
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
