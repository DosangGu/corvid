import AppKit
import Foundation

let args = CommandLine.arguments
let app = URL(fileURLWithPath: args[2], isDirectory: true).resolvingSymlinksInPath()
let bookmark = URL(fileURLWithPath: args[3])
let identifier = Bundle(url: app)!.bundleIdentifier!
let prefs = UserDefaults(suiteName: "com.apple.dock")!
let label = app.deletingPathExtension().lastPathComponent

func tiles() -> [[String: Any]] {
    prefs.synchronize()
    return prefs.array(forKey: "persistent-apps") as? [[String: Any]] ?? []
}

func data(_ tile: [String: Any]) -> [String: Any] {
    tile["tile-data"] as? [String: Any] ?? [:]
}

func writeTiles(_ values: [[String: Any]]) {
    prefs.set(values, forKey: "persistent-apps")
    prefs.synchronize()
}

func reloadDock() throws {
    let command = Process()
    command.executableURL = URL(fileURLWithPath: "/usr/bin/killall")
    command.arguments = ["Dock"]
    try command.run()
    command.waitUntilExit()
}

func applications() -> [NSRunningApplication] {
    NSRunningApplication.runningApplications(withBundleIdentifier: identifier).filter {
        !$0.isTerminated && $0.bundleURL?.resolvingSymlinksInPath().path == app.path
    }
}

func application() -> NSRunningApplication? { applications().first }

func waitForApplication() -> Bool {
    for _ in 0..<100 {
        if application() != nil { return true }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.1))
    }
    return false
}

func quit() {
    if let process = application() {
        process.terminate()
        for _ in 0..<100 {
            if application() == nil { return }
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.1))
        }
        process.forceTerminate()
    }
}

switch args[1] {
case "save-bookmark":
    try app.bookmarkData().write(to: bookmark)
case "resolve-bookmark":
    var stale = false
    let resolved = try URL(resolvingBookmarkData: Data(contentsOf: bookmark), options: .withoutUI,
                           relativeTo: nil, bookmarkDataIsStale: &stale)
    print(resolved.resolvingSymlinksInPath().path)
case "pin":
    let reference = try app.bookmarkData()
    try reference.write(to: bookmark)
    let guid = Int.random(in: 1...Int(Int32.max))
    writeTiles(tiles() + [[
        "GUID": guid,
        "tile-type": "file-tile",
        "tile-data": [
            "file-label": label,
            "file-type": 32,
            "bundle-identifier": identifier,
            "file-data": ["_CFURLString": app.absoluteString, "_CFURLStringType": 15],
            "book": reference,
        ],
    ]])
    try reloadDock()
    print(guid)
case "resolve":
    let guid = Int(args[4])!
    let tile = tiles().first { ($0["GUID"] as? Int) == guid }!
    let saved = try data(tile)["book"] as? Data ?? Data(contentsOf: bookmark)
    var stale = false
    let resolved = try URL(resolvingBookmarkData: saved, options: .withoutUI,
                           relativeTo: nil, bookmarkDataIsStale: &stale)
    precondition(resolved.resolvingSymlinksInPath().path == app.path)

case "handoff":
    // Emulate the published updater. Hold the helper's parent alive until the
    // old GUI quits, so early foreground registration is observable.
    let original = application()!
    let replacement = Process()
    replacement.executableURL = app.appendingPathComponent(args[4])
    let ready = URL(fileURLWithPath: args[5]).appendingPathComponent("ready")
    replacement.arguments = ["--update-ready-directory", args[5]] + Array(args.dropFirst(6))
    replacement.standardOutput = FileHandle.nullDevice
    try replacement.run()
    for _ in 0..<200 {
        precondition(replacement.isRunning)
        precondition(applications().map(\.processIdentifier) == [original.processIdentifier],
                     "A second GUI registered while the original was still running")
        if FileManager.default.fileExists(atPath: ready.path) { break }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
    }
    precondition(FileManager.default.fileExists(atPath: ready.path), "Relaunch handoff timed out")
    quit()
    precondition(application() == nil)
case "running":
    precondition(waitForApplication(), "Corvid did not launch from the Dock")
    precondition(applications().count == 1, "Multiple running applications use the installed bundle")
case "quit":
    quit()
    precondition(application() == nil)
case "remove":
    quit()
    writeTiles(tiles().filter { data($0)["file-label"] as? String != label })
    try reloadDock()
default:
    fatalError("Unknown Dock fixture operation")
}
