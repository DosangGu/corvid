import AppKit
import CoreServices
import Foundation

struct RelaunchError: LocalizedError {
    let errorDescription: String?
    init(_ message: String) { errorDescription = message }
}

@main
struct Relaunch {
    static func installedApp() throws -> URL {
        // This executable lives in the installed app's Contents/Helpers.
        let helper = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        let contents = helper.deletingLastPathComponent().deletingLastPathComponent()
        let app = contents.deletingLastPathComponent()
        guard let binary = Bundle(url: app)?.executableURL,
              FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw RelaunchError("The installed Desktop executable is missing.")
        }
        return app
    }

    @MainActor
    static func launch(_ app: URL, arguments: [String], startup: URL,
                       previousPID: pid_t) async throws -> NSRunningApplication {
        let config = NSWorkspace.OpenConfiguration()
        config.allowsRunningApplicationSubstitution = false
        config.promptsUserIfNeeded = false
        config.arguments = arguments
        config.environment = ["TMPDIR": startup.deletingLastPathComponent().path]

        let deadline = ContinuousClock.now + .seconds(5)
        var delay = Duration.milliseconds(200)
        while true {
            // Updates exchange Contents in place to preserve the Dock's bookmark.
            // Refresh the record for that same URL before requesting the new executable.
            _ = LSRegisterURL(app as CFURL, true)
            do {
                let running = try await NSWorkspace.shared.openApplication(at: app, configuration: config)
                guard running.processIdentifier != previousPID else {
                    throw RelaunchError("macOS still reports the previous Desktop as running.")
                }
                return running
            } catch {
                guard ContinuousClock.now + delay <= deadline else { throw error }
                FileHandle.standardError.write(Data("Retrying Desktop launch: \(error.localizedDescription)\n".utf8))
                try await Task.sleep(for: delay)
                delay = min(delay * 2, .seconds(1))
            }
        }
    }

    @MainActor
    static func waitForWindows(_ running: NSRunningApplication, startup: URL) async throws {
        let ready = startup.appendingPathComponent("ready")
        let deadline = ContinuousClock.now + .seconds(20)
        while !FileManager.default.fileExists(atPath: ready.path) {
            guard !running.isTerminated, ContinuousClock.now < deadline else {
                throw RelaunchError("Desktop did not confirm that its windows were restored.")
            }
            try await Task.sleep(for: .milliseconds(50))
        }
    }

    @MainActor
    static func handoff() async throws {
        let app = try installedApp()
        var args = Array(CommandLine.arguments.dropFirst())
        guard let index = args.firstIndex(of: "--update-ready-directory"), index + 1 < args.count else {
            throw RelaunchError("Missing update handoff directory.")
        }
        let handoff = URL(fileURLWithPath: args[index + 1], isDirectory: true)
        let temp = ProcessInfo.processInfo.environment["TMPDIR"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        } ?? FileManager.default.temporaryDirectory
        guard handoff.deletingLastPathComponent().standardizedFileURL.path == temp.standardizedFileURL.path,
              handoff.lastPathComponent.hasPrefix("corvid-desktop-ready-") else {
            throw RelaunchError("Invalid update handoff directory.")
        }

        let startup = temp.appendingPathComponent("corvid-desktop-ready-\(UUID())", isDirectory: true)
        try FileManager.default.createDirectory(at: startup, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: startup) }
        args[index] = "--window-ready-directory"
        args[index + 1] = startup.path

        let parent = getppid()
        try Data().write(to: handoff.appendingPathComponent("ready"), options: .withoutOverwriting)
        // Reparenting follows the exit of this exact caller, even if its PID is reused.
        while parent != 1 && getppid() == parent {
            try await Task.sleep(for: .milliseconds(50))
        }

        do {
            let running = try await launch(app, arguments: args, startup: startup, previousPID: parent)
            try await waitForWindows(running, startup: startup)
        } catch {
            // The old GUI can report errors only up to the handoff acknowledgment.
            NSApplication.shared.setActivationPolicy(.accessory)
            let alert = NSAlert()
            alert.messageText = "Could not reopen Corvid after the update"
            alert.informativeText = "\(error.localizedDescription) You can reopen \(app.path) in Finder."
            alert.runModal()
            throw error
        }
    }

    static func main() async {
        do {
            try await handoff()
        } catch {
            FileHandle.standardError.write(Data("Corvid relaunch failed: \(error.localizedDescription)\n".utf8))
            exit(EXIT_FAILURE)
        }
    }
}
