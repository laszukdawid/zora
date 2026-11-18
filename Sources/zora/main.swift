import AppKit
import Foundation
import ZoraCore
import ZoraMac

struct CLIError: Error, CustomStringConvertible {
    let description: String
    init(_ d: String) { description = d }
}

let usage = """
usage: zora <command> [options]

  run [--dry-run] [--file <audio>]  Run Zora (starts the core, zorad). --file feeds an audio file instead of
                                    the microphone and stops afterwards (testing).
  setup [--project <dir>]    Grant microphone + notification access, install the speech model, write config,
                             and set the project folder your coding agent works in (asks if not given).
  install | uninstall        Start/stop Zora at login (SMAppService; run from Zora.app).
  status                     Show permissions, speech model, login item and the dictation monitor.

  --config <path>            Config file (default ~/.config/zora/config.json).

  Transcript, jobs and simulation live in the core: `Zora.app/Contents/MacOS/zorad help`.
"""

func stderr(_ s: String) { FileHandle.standardError.write(Data((s + "\n").utf8)) }

func loadConfig(_ path: String?, createIfMissing: Bool) throws -> ZoraConfig {
    let url = path.map { URL(fileURLWithPath: expandTilde($0)) } ?? ZoraConfig.defaultURL
    if !FileManager.default.fileExists(atPath: url.path) {
        guard createIfMissing else { return ZoraConfig() }
        try ZoraConfig().write(to: url)
        stderr("wrote default config to \(url.path); set harnesses.claude.workspace before running without --dry-run")
    }
    do { return try ZoraConfig.load(from: url) } catch { throw CLIError("cannot read \(url.path): \(error)") }
}

func parseDuration(_ s: String) -> Double? {
    guard let unit = s.last, let n = Double(s.dropLast()) else { return Double(s) }
    switch unit { case "s": return n; case "m": return n * 60; case "h": return n * 3600; default: return Double(s) }
}

/// Runs `body` on the main run loop (needed for notifications and NSWorkspace) and exits with its result.
func runMain(_ body: @escaping () async throws -> Int32) -> Never {
    Task { @MainActor in
        do { exit(try await body()) } catch {
            stderr("zora: \(error)")
            exit(1)
        }
    }
    NSApplication.shared.setActivationPolicy(.accessory)
    NSApplication.shared.run()
    exit(0)
}

var args = Array(CommandLine.arguments.dropFirst())
var configPath: String?
if let i = args.firstIndex(of: "--config"), i + 1 < args.count {
    configPath = args[i + 1]
    args.removeSubrange(i...(i + 1))
}
let command = args.first ?? "run"
let rest = Array(args.dropFirst())

func zoradPath() -> String {
    if let p = ProcessInfo.processInfo.environment["ZORA_ZORAD_PATH"] { return p }
    return Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/zorad").path
}

switch command {
case "run":
    // One listener at a time: a second copy (e.g. opened by Launch Services) exits immediately.
    let me = ProcessInfo.processInfo.processIdentifier
    if let id = Bundle.main.bundleIdentifier,
       NSRunningApplication.runningApplications(withBundleIdentifier: id).contains(where: { $0.processIdentifier != me }) {
        stderr("zora: already running")
        exit(0)
    }
    // In the background (launchd), keep metadata-only logs where they can be found.
    if isatty(STDERR_FILENO) == 0 {
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Zora")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let log = dir.appendingPathComponent("zora.log")
        if let size = try? FileManager.default.attributesOfItem(atPath: log.path)[.size] as? Int, size > 5_000_000 {
            try? FileManager.default.removeItem(at: log)
        }
        let fd = open(log.path, O_WRONLY | O_CREAT | O_APPEND, 0o600)
        if fd >= 0 { dup2(fd, STDERR_FILENO); close(fd) }
    }
    var coreArgs: [String] = []
    if rest.contains("--dry-run") { coreArgs.append("--dry-run") }
    if let configPath { coreArgs += ["--config", configPath] }
    var file: URL?
    if let i = rest.firstIndex(of: "--file"), i + 1 < rest.count { file = URL(fileURLWithPath: rest[i + 1]) }
    guard FileManager.default.isExecutableFile(atPath: zoradPath()) else {
        stderr("zora: core not found at \(zoradPath()) (set ZORA_ZORAD_PATH or build Zora.app)")
        exit(1)
    }
    let bridge = Bridge(options: .init(zorad: zoradPath(), coreArgs: coreArgs, audioFile: file))
    NSApplication.shared.setActivationPolicy(.accessory)
    MainActor.assumeIsolated { bridge.startIndicator() }
    bridge.run()
    NSApplication.shared.run()

case "setup":
    runMain {
        var config = try loadConfig(configPath, createIfMissing: true)
        print("microphone:", await Microphone.request() ? "granted" : "denied")
        let mac = MacNotifier()
        print("notifications:", mac.isAvailable ? (await mac.requestAuthorization() ? "authorized" : "denied") : "unavailable (run from Zora.app)")
        print("speech model for \(config.locale): installing if needed…")
        let locale = try await SpeechPipeline.installAssets(for: Locale(identifier: config.locale)) { p in
            FileHandle.standardError.write(Data(String(format: "\r  %3.0f%%", p * 100).utf8))
        }
        print("\nspeech model: installed (\(locale.identifier))")
        var dir: String?
        if let i = rest.firstIndex(of: "--project"), i + 1 < rest.count { dir = rest[i + 1] }
        if dir != nil || config.validate().contains(where: { $0.contains("workspace") }) {
            if dir == nil, isatty(STDIN_FILENO) != 0 {
                print("Project folder for your coding agent (it runs there and can change files): ", terminator: "")
                dir = readLine()?.trimmingCharacters(in: .whitespaces)
            }
            if let dir, !dir.isEmpty {
                let path = URL(fileURLWithPath: expandTilde(dir)).standardizedFileURL.path
                let change = String(decoding: try JSONSerialization.data(withJSONObject: ["workspace": path]), as: UTF8.self)
                // Saved through the core, which validates it the same way the menu bar's Settings does.
                let r = await ZoradCLI(path: zoradPath(), configPath: configPath).run(["settings", "set", change])
                guard r.ok else { print("project folder:", r.message); return 1 }
                print("project folder:", path)
                config = try loadConfig(configPath, createIfMissing: false)
            }
        }
        let problems = config.validate()
        problems.forEach { print("config:", $0) }
        return problems.isEmpty ? 0 : 1
    }

case "install":
    runMain {
        try LoginItem.register()
        print("login item:", LoginItem.status)
        return 0
    }

case "uninstall":
    runMain {
        try LoginItem.unregister()
        print("login item:", LoginItem.status)
        return 0
    }

case "status":
    runMain {
        let config = try loadConfig(configPath, createIfMissing: false)
        print("bundle:", Bundle.main.bundleIdentifier ?? "none (not running from Zora.app)")
        print("core:", FileManager.default.isExecutableFile(atPath: zoradPath()) ? zoradPath() : "missing")
        print("microphone:", Microphone.status)
        print("notifications:", await MacNotifier().authorizationDescription())
        let (locale, asset) = await SpeechPipeline.assetStatus(for: Locale(identifier: config.locale))
        print("speech model:", locale?.identifier ?? "unsupported locale \(config.locale)", "\(asset)")
        print("login item:", LoginItem.status)
        if let snap = CoreAudioDictationMonitor.snapshot(bundleIDs: config.dictation.bundleIDs) {
            print("dictation apps:", snap.isEmpty ? "none running" : snap.map { "\($0.bundleID) pid \($0.pid) capturing=\($0.capturing)" }.joined(separator: ", "))
        } else {
            print("dictation apps: cannot read CoreAudio process list")
        }
        return 0
    }

case "-h", "--help", "help":
    print(usage)

default:
    stderr(usage)
    exit(2)
}
