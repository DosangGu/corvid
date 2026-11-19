import AppKit
import Foundation

let args = CommandLine.arguments
let app = URL(fileURLWithPath: args[2], isDirectory: true).resolvingSymlinksInPath()
let bookmark = URL(fileURLWithPath: args[3])
let identifier = "org.corvid.desktop"
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

func application() -> NSRunningApplication? {
    NSRunningApplication.runningApplications(withBundleIdentifier: identifier).first {
        !$0.isTerminated && $0.bundleURL?.resolvingSymlinksInPath().path == app.path
    }
}

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

case "running":
    precondition(waitForApplication(), "Corvid did not launch from the Dock")
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
