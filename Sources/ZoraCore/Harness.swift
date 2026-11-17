import Foundation

public struct HarnessResult: Sendable, Equatable {
    public var success: Bool
    public var exitCode: Int32?
    public var summary: String

    public init(success: Bool, exitCode: Int32?, summary: String) {
        self.success = success
        self.exitCode = exitCode
        self.summary = summary
    }
}

/// An AI agent backend that executes one prompt. Cancellation of the calling task must stop the work.
public protocol Harness: Sendable {
    var id: String { get }
    /// Jobs on the same workspace run one at a time.
    var workspace: String { get }
    func run(prompt: String, timeout: Double) async -> HarnessResult
}

/// Runs `executable args…` with "{prompt}" replaced by the prompt as a single argument. No shell is involved.
public struct ArgvHarness: Harness {
    public let id: String
    public let workspace: String
    public let executable: String
    public let arguments: [String]
    public let environment: [String: String]
    public let searchPath: [String]
    public let summarize: @Sendable (ProcessOutcome) -> HarnessResult

    public init(
        id: String, workspace: String, executable: String, arguments: [String],
        environment: [String: String] = [:], searchPath: [String],
        summarize: (@Sendable (ProcessOutcome) -> HarnessResult)? = nil
    ) {
        self.id = id
        self.workspace = expandTilde(workspace)
        self.executable = executable
        self.arguments = arguments
        self.environment = environment
        self.searchPath = searchPath
        self.summarize = summarize ?? ArgvHarness.lastLineSummary
    }

    public func argv(for prompt: String) -> [String] {
        arguments.map { $0 == "{prompt}" ? prompt : $0 }
    }

    public func run(prompt: String, timeout: Double) async -> HarnessResult {
        guard let exe = ProcessRunner.resolve(executable, searchPath: searchPath) else {
            return HarnessResult(success: false, exitCode: nil, summary: "\(executable) not found on PATH")
        }
        var env = ProcessInfo.processInfo.environment
        env["PATH"] = searchPath.joined(separator: ":")
        env.merge(environment) { _, new in new }
        let outcome = await ProcessRunner.run(
            executable: exe, arguments: argv(for: prompt), cwd: workspace, environment: env, timeout: timeout
        )
        switch outcome.termination {
        case .timedOut: return HarnessResult(success: false, exitCode: nil, summary: "timed out after \(Int(timeout))s")
        case .cancelled: return HarnessResult(success: false, exitCode: nil, summary: "cancelled")
        case .spawnFailed(let e): return HarnessResult(success: false, exitCode: nil, summary: "could not start \(executable): \(String(cString: strerror(e)))")
        case .signaled(let s): return HarnessResult(success: false, exitCode: nil, summary: "killed by signal \(s)")
        case .exited: return summarize(outcome)
        }
    }

    @Sendable public static func lastLineSummary(_ o: ProcessOutcome) -> HarnessResult {
        guard case .exited(let code) = o.termination else { return HarnessResult(success: false, exitCode: nil, summary: "failed") }
        let source = o.stdout.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? o.stderr : o.stdout
        let line = source.split(whereSeparator: \.isNewline).last.map(String.init) ?? "no output"
        return HarnessResult(success: code == 0, exitCode: code, summary: code == 0 ? line : "exit \(code): \(line)")
    }
}

public enum ClaudeCodeHarness {
    public static func make(id: String, config h: ZoraConfig.Harness, searchPath: [String]) -> ArgvHarness {
        var args = ["-p", "{prompt}", "--output-format", "json"]
        if let mode = h.permissionMode { args += ["--permission-mode", mode] }
        if !h.allowedTools.isEmpty { args += ["--allowedTools"] + h.allowedTools }
        args += h.extraArgs
        return ArgvHarness(
            id: id, workspace: h.workspace, executable: h.executable, arguments: args,
            environment: h.env, searchPath: searchPath, summarize: summarize
        )
    }

    @Sendable static func summarize(_ o: ProcessOutcome) -> HarnessResult {
        guard case .exited(let code) = o.termination else { return ArgvHarness.lastLineSummary(o) }
        struct Out: Decodable { var result: String?; var is_error: Bool? }
        if let data = o.stdout.data(using: .utf8), let out = try? JSONDecoder().decode(Out.self, from: data) {
            let ok = code == 0 && out.is_error != true
            let text = (out.result ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return HarnessResult(success: ok, exitCode: code, summary: text.isEmpty ? (ok ? "done" : "failed") : text)
        }
        return ArgvHarness.lastLineSummary(o)
    }
}

/// Reports what would run without executing anything.
public struct DryRunHarness: Harness {
    public let id: String
    public let workspace: String
    public init(id: String, workspace: String) { self.id = id; self.workspace = workspace }
    public func run(prompt: String, timeout: Double) async -> HarnessResult {
        HarnessResult(success: true, exitCode: 0, summary: "dry run: \(prompt)")
    }
}

public enum HarnessFactory {
    public static func make(config: ZoraConfig) -> [String: any Harness] {
        var out: [String: any Harness] = [:]
        for (name, h) in config.harnesses {
            if config.dryRun {
                out[name] = DryRunHarness(id: name, workspace: expandTilde(h.workspace))
            } else if h.type == "claude" {
                out[name] = ClaudeCodeHarness.make(id: name, config: h, searchPath: config.searchPath)
            } else {
                out[name] = ArgvHarness(
                    id: name, workspace: h.workspace, executable: h.executable, arguments: h.args,
                    environment: h.env, searchPath: config.searchPath
                )
            }
        }
        return out
    }
}
