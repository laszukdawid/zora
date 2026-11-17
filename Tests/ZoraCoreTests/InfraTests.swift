import Foundation
import Testing
@testable import ZoraCore

private let searchPath = ["/usr/bin", "/bin"]

@Suite struct HarnessTests {
    @Test func promptIsOneArgumentWithoutShell() async throws {
        let dir = FileManager.default.temporaryDirectory.path
        let h = ArgvHarness(id: "t", workspace: dir, executable: "printf", arguments: ["%s|", "{prompt}"], searchPath: searchPath)
        let r = await h.run(prompt: "a b; rm -rf / $(whoami) `id`", timeout: 10)
        #expect(r.success)
        #expect(r.summary == "a b; rm -rf / $(whoami) `id`|")
    }

    @Test func runsInWorkspace() async {
        let h = ArgvHarness(id: "t", workspace: "/usr", executable: "pwd", arguments: ["{prompt}"].dropLast(), searchPath: searchPath)
        #expect(await h.run(prompt: "", timeout: 10).summary == "/usr")
    }

    @Test func missingExecutable() async {
        let h = ArgvHarness(id: "t", workspace: "/tmp", executable: "no-such-zora-bin", arguments: ["{prompt}"], searchPath: searchPath)
        let r = await h.run(prompt: "x", timeout: 5)
        #expect(!r.success && r.summary.contains("not found"))
    }

    @Test func nonZeroExit() async {
        let h = ArgvHarness(id: "t", workspace: "/tmp", executable: "sh", arguments: ["-c", "echo boom >&2; exit 3", "{prompt}"], searchPath: searchPath)
        let r = await h.run(prompt: "x", timeout: 5)
        #expect(!r.success && r.exitCode == 3 && r.summary == "exit 3: boom")
    }

    @Test func claudeArgsAndJsonSummary() {
        var cfg = ZoraConfig.Harness()
        cfg.permissionMode = "acceptEdits"
        cfg.allowedTools = ["Read"]
        let h = ClaudeCodeHarness.make(id: "claude", config: cfg, searchPath: searchPath)
        #expect(h.argv(for: "do x") == ["-p", "do x", "--output-format", "json", "--permission-mode", "acceptEdits", "--allowedTools", "Read"])
        let ok = ClaudeCodeHarness.summarize(ProcessOutcome(termination: .exited(0), stdout: #"{"result":"Added tests","is_error":false}"#, stderr: "", truncated: false))
        #expect(ok == HarnessResult(success: true, exitCode: 0, summary: "Added tests"))
        let err = ClaudeCodeHarness.summarize(ProcessOutcome(termination: .exited(0), stdout: #"{"result":"nope","is_error":true}"#, stderr: "", truncated: false))
        #expect(!err.success)
    }
}

@Suite struct ProcessRunnerTests {
    @Test func timeoutKillsWholeGroupIncludingStubbornChild() async throws {
        let marker = FileManager.default.temporaryDirectory.appendingPathComponent("zora-\(UUID().uuidString)").path
        // Parent ignores TERM; child sleeps and would write the marker if it survived.
        let script = "trap '' TERM; (sleep 3; touch \(marker)) & sleep 30"
        let start = Date()
        let o = await ProcessRunner.run(executable: "/bin/sh", arguments: ["-c", script], cwd: "/tmp", environment: [:], timeout: 0.5, killGrace: 0.5)
        #expect(o.termination == .timedOut)
        #expect(Date().timeIntervalSince(start) < 6)
        try await Task.sleep(for: .seconds(3.5))
        #expect(!FileManager.default.fileExists(atPath: marker))
    }

    @Test func floodedPipesAreCappedAndDrained() async {
        let o = await ProcessRunner.run(
            executable: "/bin/sh", arguments: ["-c", "yes out | head -c 5000000; yes err | head -c 5000000 >&2"],
            cwd: "/tmp", environment: [:], timeout: 20, captureLimit: 1024
        )
        #expect(o.termination == .exited(0))
        #expect(o.truncated && o.stdout.utf8.count == 1024 && o.stderr.utf8.count == 1024)
    }

    @Test func childGetsDefaultSignalHandlingEvenIfParentIgnoresTerm() async {
        signal(SIGTERM, SIG_IGN)
        defer { signal(SIGTERM, SIG_DFL) }
        let start = Date()
        let o = await ProcessRunner.run(executable: "/bin/sleep", arguments: ["30"], cwd: "/tmp", environment: [:], timeout: 0.3, killGrace: 10)
        #expect(o.termination == .timedOut)
        // SIGTERM must work; waiting for the 10 s SIGKILL grace would mean the child inherited SIG_IGN.
        #expect(Date().timeIntervalSince(start) < 3)
    }

    @Test func cancellationStopsProcess() async {
        let task = Task { await ProcessRunner.run(executable: "/bin/sleep", arguments: ["30"], cwd: "/tmp", environment: [:], timeout: 60, killGrace: 0.5) }
        try? await Task.sleep(for: .milliseconds(200))
        task.cancel()
        #expect(await task.value.termination == .cancelled)
    }
}

@Suite struct DispatcherTests {
    final class SlowHarness: Harness, @unchecked Sendable {
        let id: String
        let workspace: String
        private let lock = NSLock()
        private var active = 0
        private(set) var maxActive = 0
        init(id: String, workspace: String) { self.id = id; self.workspace = workspace }
        func run(prompt: String, timeout: Double) async -> HarnessResult {
            lock.withLock { active += 1; maxActive = max(maxActive, active) }
            try? await Task.sleep(for: .milliseconds(100))
            lock.withLock { active -= 1 }
            return HarnessResult(success: true, exitCode: 0, summary: prompt)
        }
    }

    @Test func serializesPerWorkspaceAndBoundsQueue() async {
        let h = SlowHarness(id: "a", workspace: "/w")
        let d = Dispatcher(timeout: 10, maxQueue: 2, maxAge: 60) { _ in }
        var accepted = 0
        for i in 0..<5 { if await d.submit(id: UUID(), prompt: "\(i)", harness: h) { accepted += 1 } }
        // First starts immediately; two more fit in the queue.
        #expect(accepted == 3)
        await d.drain()
        #expect(h.maxActive == 1)
    }

    @Test func differentWorkspacesRunConcurrently() async {
        let a = SlowHarness(id: "a", workspace: "/a"), b = SlowHarness(id: "b", workspace: "/b")
        let d = Dispatcher(timeout: 10, maxQueue: 4, maxAge: 60) { _ in }
        let start = Date()
        _ = await d.submit(id: UUID(), prompt: "1", harness: a)
        _ = await d.submit(id: UUID(), prompt: "2", harness: b)
        await d.drain()
        #expect(Date().timeIntervalSince(start) < 0.19)
    }
}

@Suite struct ConfigTests {
    @Test func defaultsFillMissingKeys() throws {
        let c = try JSONDecoder().decode(ZoraConfig.self, from: Data(#"{"capture":{"maxChars":10},"harnesses":{"c":{"workspace":"/tmp"}},"defaultHarness":"c"}"#.utf8))
        #expect(c.capture.maxChars == 10 && c.capture.holdBackSeconds == 1.5)
        #expect(c.harnesses["c"]?.executable == "claude")
        #expect(c.validate().isEmpty)
    }

    @Test func rejectsPermissionBypass() {
        var c = ZoraConfig()
        c.harnesses["claude"]?.workspace = "/tmp"
        c.harnesses["claude"]?.extraArgs = ["--dangerously-skip-permissions"]
        c.harnesses["x"] = { var h = ZoraConfig.Harness(); h.type = "argv"; h.args = ["--permission-mode", "bypassPermissions", "{prompt}"]; return h }()
        let p = c.validate()
        #expect(p.count == 2)
        c.harnesses["claude"]?.extraArgs = []
        c.harnesses["x"] = nil
        c.harnesses["claude"]?.permissionMode = "bypassPermissions"
        #expect(c.validate().count == 2)
    }

    @Test func rejectsBypassSmuggledThroughAllowedTools() {
        var c = ZoraConfig()
        c.dryRun = true
        c.harnesses["claude"]?.allowedTools = ["Read", "--permission-mode=bypassPermissions"]
        #expect(c.validate().count == 2)
        c.harnesses["claude"]?.allowedTools = ["Read", "--model"]
        #expect(c.validate().count == 1)
    }

    @Test func requiresWorkspaceUnlessDryRun() {
        var c = ZoraConfig()
        #expect(!c.validate().isEmpty)
        c.dryRun = true
        #expect(c.validate().isEmpty)
    }

    @Test func roundTrips() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("zora-\(UUID()).json")
        var c = ZoraConfig()
        c.locale = "pl-PL"
        try c.write(to: url)
        #expect(try ZoraConfig.load(from: url) == c)
    }
}

@Suite struct StoreTests {
    @Test func batchesRetainsAndPrunesBothTables() async throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("zora-\(UUID())/z.sqlite").path
        let store = try SQLiteStore(path: path)
        let now = HostClock.now()
        await store.append(TranscriptSegment(session: 1, start: now - 7200, end: now - 7190, text: "old"), kind: .speech)
        await store.append(TranscriptSegment(session: 1, start: now - 5, end: now - 4, text: "new"), kind: .speech)
        await store.recordCommand(id: UUID(), text: "cmd", harness: "h", status: "queued")
        #expect(await store.segments(since: .distantPast).map(\.text) == ["old", "new"])
        await store.maintenance(retention: 3600)
        #expect(await store.segments(since: .distantPast).map(\.text) == ["new"])
        #expect(await store.count("commands") == 1)
        await store.maintenance(retention: 0.0001)
        #expect(await store.count("commands") == 0)
        #expect(await store.takeFailure() == nil)
        let perms = try FileManager.default.attributesOfItem(atPath: path)[.posixPermissions] as? Int
        #expect(perms == 0o600)
    }

    @Test func retagsDictationAndMarksInterruptedJobs() async throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("zora-\(UUID())/z.sqlite").path
        var store: SQLiteStore? = try SQLiteStore(path: path)
        let now = HostClock.now()
        await store!.append(TranscriptSegment(session: 1, start: now - 2, end: now - 1, text: "dictated"), kind: .speech)
        await store!.retagDictation(from: Date().addingTimeInterval(-3), to: Date())
        #expect(await store!.segments(since: .distantPast).map(\.kind) == [.dictation])
        await store!.recordCommand(id: UUID(), text: "c", harness: "h", status: "running")
        store = nil
        let reader = try SQLiteStore(path: path, readOnly: true)
        #expect(await reader.commands(since: .distantPast).map(\.status) == ["running"])
        let reopened = try SQLiteStore(path: path)
        await reopened.recoverInterruptedJobs()
        #expect(await reopened.commands(since: .distantPast).map(\.status) == ["interrupted"])
    }
}
