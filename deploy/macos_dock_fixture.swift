import AppKit
import Foundation

let args = CommandLine.arguments
let app = URL(fileURLWithPath: args[2], isDirectory: true).resolvingSymlinksInPath()
let bookmark = URL(fileURLWithPath: args[3])
let bundle = Bundle(url: app)!
let identifier = bundle.bundleIdentifier!
let prefs = UserDefaults(suiteName: "com.apple.dock")!
let label = bundle.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String
    ?? bundle.object(forInfoDictionaryKey: "CFBundleName") as? String
    ?? app.deletingPathExtension().lastPathComponent

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

func describe(_ apps: [NSRunningApplication]) -> String {
    apps.map {
        "pid=\($0.processIdentifier) executable=\($0.executableURL?.path ?? "?") bundle=\($0.bundleURL?.path ?? "?")"
    }.joined(separator: ", ")
}

func singleApplication(_ phase: String) -> NSRunningApplication? {
    let apps = applications()
    precondition(apps.count <= 1, "\(phase): found \(apps.count) running apps: \(describe(apps))")
    return apps.first
}

func waitForApplication(after previous: pid_t?) -> NSRunningApplication {
    let deadline = ContinuousClock.now + .seconds(10)
    repeat {
        if let current = singleApplication("Waiting for Desktop"),
           current.processIdentifier != previous {
            return current
        }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
    } while ContinuousClock.now < deadline
    fatalError("Desktop did not register a new process (previous PID \(String(describing: previous))); found: \(describe(applications()))")
}

func waitForExit(_ apps: [NSRunningApplication], observing: () -> Void) -> Bool {
    let deadline = ContinuousClock.now + .seconds(10)
    repeat {
        observing()
        if apps.allSatisfy(\.isTerminated) { return true }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
    } while ContinuousClock.now < deadline
    return false
}

func quit(_ apps: [NSRunningApplication], observing: () -> Void = {}) {
    for app in apps { app.terminate() }
    if waitForExit(apps, observing: observing) { return }
    for app in apps where !app.isTerminated { app.forceTerminate() }
    precondition(waitForExit(apps, observing: observing),
                 "Desktop did not quit: \(describe(apps.filter { !$0.isTerminated }))")
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
    guard let original = singleApplication("Before relaunch handoff") else {
        fatalError("The original Desktop is missing")
    }
    let originalPID = original.processIdentifier
    let replacement = Process()
    replacement.executableURL = app.appendingPathComponent(args[4])
    let ready = URL(fileURLWithPath: args[5]).appendingPathComponent("ready")
    replacement.arguments = ["--update-ready-directory", args[5]] + Array(args.dropFirst(6))
    replacement.standardOutput = FileHandle.nullDevice
    try replacement.run()
    for _ in 0..<200 {
        precondition(replacement.isRunning)
        let current = singleApplication("Before relaunch acknowledgment")
        precondition(current?.processIdentifier == originalPID,
                     "Original Desktop PID \(originalPID) changed: \(describe([current].compactMap { $0 }))")
        if FileManager.default.fileExists(atPath: ready.path) { break }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05))
    }
    precondition(FileManager.default.fileExists(atPath: ready.path), "Relaunch handoff timed out")
    quit([original]) {
        let current = singleApplication("During relaunch handoff")
        if let current, !original.isTerminated {
            precondition(current.processIdentifier == originalPID,
                         "Replacement registered before original PID \(originalPID) exited: \(describe([current]))")
        }
    }
    print(originalPID)
case "running":
    let previous = args.count > 4 ? pid_t(args[4])! : nil
    print(waitForApplication(after: previous).processIdentifier)
case "quit":
    quit(applications())
    precondition(applications().isEmpty)
case "remove":
    let guid = Int(args[4])!
    quit(applications())
    precondition(applications().isEmpty, "The Dock fixture is still running")
    writeTiles(tiles().filter { ($0["GUID"] as? Int) != guid })
    try reloadDock()
default:
    fatalError("Unknown Dock fixture operation")
}
