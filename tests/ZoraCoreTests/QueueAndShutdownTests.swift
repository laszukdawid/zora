import Foundation
import Testing
@testable import ZoraCore

@Suite struct InputQueueTests {
    private func drain(_ q: InputQueue, _ n: Int) async -> [Input] {
        var out: [Input] = []
        var it = q.stream.makeAsyncIterator()
        while out.count < n, let x = await it.next() { q.consumed(x); out.append(x) }
        return out
    }

    private func label(_ i: Input) -> String {
        switch i {
        case .transcript(.segment(let s)): s.text
        case .transcript(.gap): "gap"
        case .dictation: "dictation"
        case .tick: "tick"
        case .lossMarker: "LOSS"
        }
    }

    private func seg(_ t: String) -> Input { .transcript(.segment(TranscriptSegment(session: 1, start: 0, end: 1, text: t))) }

    @Test func lossMarkerSitsWhereTheEventWasDropped() async {
        let q = InputQueue(limit: 2)
        q.send(seg("hey zora"))
        q.send(seg("filler"))
        q.send(seg("zora cancel"))      // dropped
        q.send(seg("also dropped"))     // dropped, no second marker
        q.send(.dictation(.began(1)))   // never dropped
        let got = await drain(q, 4).map(label)
        #expect(got == ["hey zora", "filler", "LOSS", "dictation"])
        q.send(seg("do it zora"))       // room again after consuming
        #expect(await drain(q, 1).map(label) == ["do it zora"])
    }
}

@Suite struct ShutdownTests {
    /// Store whose recordCommand suspends until released, to interleave shutdown with a dispatch.
    actor GateStore: TranscriptStoring {
        private var gate: CheckedContinuation<Void, Never>?
        private var entered: CheckedContinuation<Void, Never>?
        private var hasEntered = false
        var finished: [String] = []
        func waitUntilEntered() async {
            if hasEntered { return }
            await withCheckedContinuation { entered = $0 }
        }
        func release() { gate?.resume(); gate = nil }
        func recordCommand(id: UUID, text: String, harness: String, status: String) async {
            hasEntered = true
            entered?.resume(); entered = nil
            await withCheckedContinuation { gate = $0 }
        }
        func finishCommand(id: UUID, status: String, exitCode: Int32?, summary: String) { finished.append(status) }
        func append(_ segment: TranscriptSegment, kind: SegmentKind) {}
        func retagDictation(from: Date, to: Date) {}
        func maintenance(retention: Double) {}
        func flush() {}
        func flushIfDue() {}
        func takeFailure() -> String? { nil }
    }

    @Test func dispatchSuspendedInStoreDoesNotRunAfterShutdown() async {
        let store = GateStore()
        let harness = RecordingHarness()
        let clock = FakeClock()
        var c = ZoraConfig()
        c.capture.holdBackSeconds = 0
        let coordinator = Coordinator(config: c, store: store, notifier: NotifierHub(notifiers: [], routing: [:]), harness: harness, now: { clock.now })
        await coordinator.start()
        await coordinator.handle(.health(healthy: true, reason: "ok"))
        await coordinator.handle(.segment(TranscriptSegment(session: 1, start: clock.now - 2, end: clock.now - 1, text: "Hey Zora x. Do it Zora")))
        let tick = Task { await coordinator.tick() }
        await store.waitUntilEntered()
        await coordinator.shutdown(timeout: 1)
        await store.release()
        await tick.value
        try? await Task.sleep(for: .milliseconds(100))
        #expect(harness.prompts.isEmpty)
        #expect(await store.finished == ["cancelled"])
    }

    @Test func closedDispatcherRejects() async {
        let d = Dispatcher(timeout: 5, maxQueue: 4, maxAge: 60) { _ in }
        await d.close()
        #expect(!(await d.submit(id: UUID(), prompt: "x", harness: RecordingHarness())))
    }

    @Test func cancelledJobReturnsOnlyAfterStubbornDescendantDies() async throws {
        let pidFile = FileManager.default.temporaryDirectory.appendingPathComponent("zora-\(UUID().uuidString)").path
        // Leader exits on TERM; its child ignores TERM, closes the pipes and keeps running.
        // `$$` in a subshell is the parent's pid; ask a child for its PPID to get the subshell's own pid.
        let script = "(trap '' TERM; exec >/dev/null 2>&1; sh -c 'echo $PPID' > \(pidFile); exec sleep 60) & sleep 60"
        let task = Task {
            await ProcessRunner.run(executable: "/bin/sh", arguments: ["-c", script], cwd: "/tmp", environment: [:], timeout: 60, killGrace: 0.5)
        }
        try await Task.sleep(for: .milliseconds(400))
        task.cancel()
        _ = await task.value
        let raw = try String(contentsOfFile: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
        let child = pid_t(raw) ?? 0
        #expect(child > 0)
        #expect(kill(child, 0) == -1 && errno == ESRCH)
    }
}
