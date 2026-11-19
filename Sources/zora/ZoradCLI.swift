import Foundation
import os

/// Runs the bundled `zorad` for the settings the menu bar changes, so pause state and config validation live in
/// one place for every platform's tray.
struct ZoradCLI: Sendable {
    let path: String
    let configPath: String?
    var timeout: TimeInterval = 10

    struct Result: Sendable {
        let code: Int32
        let out: String
        let err: String
        /// No answer in time: the change may or may not have been saved.
        var timedOut = false
        var ok: Bool { code == 0 }
        /// The error lines, or a fallback when `zorad` printed none.
        var message: String {
            let e = err.trimmingCharacters(in: .whitespacesAndNewlines)
            return e.isEmpty ? "zorad exited with status \(code)" : e
        }
    }

    func run(_ args: [String]) async -> Result {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = (configPath.map { ["--config", $0] } ?? []) + args
        let out = Pipe(), err = Pipe()
        process.standardOutput = out
        process.standardError = err
        process.standardInput = FileHandle.nullDevice
        let timeout = self.timeout
        let path = self.path
        return await withCheckedContinuation { cont in
            DispatchQueue.global(qos: .userInitiated).async {
                do { try process.run() } catch {
                    cont.resume(returning: Result(code: -1, out: "", err: "cannot run \(path): \(error.localizedDescription)"))
                    return
                }
                // A hung child must not leave the menu or the settings window disabled for good.
                let timedOut = OSAllocatedUnfairLock(initialState: false)
                let pid = process.processIdentifier
                let timer = DispatchWorkItem {
                    guard process.isRunning else { return }
                    timedOut.withLock { $0 = true }
                    process.terminate()
                    DispatchQueue.global().asyncAfter(deadline: .now() + 2) {
                        if process.isRunning { kill(pid, SIGKILL) }
                    }
                }
                DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: timer)
                // Drain both pipes while it runs, so neither can fill up and block the child.
                let group = DispatchGroup()
                let errData = DataBox()
                group.enter()
                DispatchQueue.global().async {
                    errData.value = err.fileHandleForReading.readDataToEndOfFile()
                    group.leave()
                }
                let o = out.fileHandleForReading.readDataToEndOfFile()
                group.wait()
                process.waitUntilExit()
                timer.cancel()
                let late = timedOut.withLock { $0 }
                cont.resume(returning: Result(
                    code: process.terminationStatus,
                    out: String(decoding: o, as: UTF8.self),
                    err: late
                        ? "zorad did not answer within \(Int(timeout)) s; the change may or may not have been saved"
                        : String(decoding: errData.value, as: UTF8.self),
                    timedOut: late))
            }
        }
    }
}

/// What `zorad settings` prints: the settings the menu bar edits, plus what the app needs to know about the config.
struct CoreSettings: Decodable, Sendable {
    struct Phrases: Decodable, Sendable { var start: [String]; var end: [String]; var cancel: [String] }
    var wakeNames: [String]
    var phrases: Phrases
    var workspace: String
    /// The default harness the workspace belongs to.
    var harness: String
    var locale: String
    var dictationBundleIDs: [String]
    var problems: [String]

    static func decode(_ out: String) -> CoreSettings? {
        try? JSONDecoder().decode(CoreSettings.self, from: Data(out.utf8))
    }
}

extension ZoradCLI {
    /// Reads the settings (creating a default config file if there is none). `dryRun` reports problems as
    /// `zorad run --dry-run` sees them.
    func settings(dryRun: Bool = false) async throws -> CoreSettings {
        let r = await run(["settings"] + (dryRun ? ["--dry-run"] : []))
        guard r.ok else { throw SettingsError(message: r.message) }
        guard let s = CoreSettings.decode(r.out) else { throw SettingsError(message: "unexpected output from zorad settings") }
        return s
    }
}

struct SettingsError: Error, CustomStringConvertible {
    let message: String
    var description: String { message }
}

/// Written by one reader thread, read only after `DispatchGroup.wait()`.
private final class DataBox: @unchecked Sendable {
    var value = Data()
}
