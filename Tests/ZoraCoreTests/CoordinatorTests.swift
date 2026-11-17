import Foundation
import Testing
@testable import ZoraCore

final class FakeClock: @unchecked Sendable {
    private let lock = NSLock()
    private var t: Double
    init(_ t: Double = 1000) { self.t = t }
    var now: Double { lock.lock(); defer { lock.unlock() }; return t }
    func advance(_ d: Double) { lock.lock(); t += d; lock.unlock() }
}

actor MemoryStore: TranscriptStoring {
    var segments: [(TranscriptSegment, SegmentKind)] = []
    var commands: [UUID: String] = [:]
    var retags = 0
    func append(_ segment: TranscriptSegment, kind: SegmentKind) { segments.append((segment, kind)) }
    func retagDictation(from: Date, to: Date) { retags += 1 }
    func recordCommand(id: UUID, text: String, harness: String, status: String) { commands[id] = status }
    func finishCommand(id: UUID, status: String, exitCode: Int32?, summary: String) { commands[id] = status }
    func maintenance(retention: Double) {}
    func flush() {}
    func flushIfDue() {}
    func takeFailure() -> String? { nil }
}

actor RecordingNotifier: Notifier {
    nonisolated let id = "rec"
    var events: [ZoraEvent] = []
    func notify(_ event: ZoraEvent) { events.append(event) }
}

final class RecordingHarness: Harness, @unchecked Sendable {
    let id = "fake"
    let workspace = "/tmp"
    private let lock = NSLock()
    private var _prompts: [String] = []
    var prompts: [String] { lock.lock(); defer { lock.unlock() }; return _prompts }
    func run(prompt: String, timeout: Double) async -> HarnessResult {
        lock.withLock { _prompts.append(prompt) }
        return HarnessResult(success: true, exitCode: 0, summary: "ok")
    }
}

struct Rig {
    let clock = FakeClock()
    let store = MemoryStore()
    let notifier = RecordingNotifier()
    let harness = RecordingHarness()
    let coordinator: Coordinator

    init(holdBack: Double = 0) {
        var c = ZoraConfig()
        c.capture.holdBackSeconds = holdBack
        let clock = self.clock
        coordinator = Coordinator(
            config: c, store: store, notifier: NotifierHub(notifiers: [notifier], routing: ["rec": ["*"]]),
            harness: harness, now: { clock.now }
        )
    }

    func say(_ text: String, at offset: Double, length: Double = 1) async {
        let start = clock.now + offset
        await coordinator.handle(.segment(TranscriptSegment(session: 1, start: start, end: start + length, text: text)))
    }

    func settle() async {
        await coordinator.tick()
        await coordinator.drainJobs()
    }
}

@Suite struct CoordinatorTests {
    @Test func dispatchesWhenHealthyAndNoDictation() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.say("Hey Zora, add a README. Do it Zora.", at: -2)
        await rig.settle()
        #expect(rig.harness.prompts == ["add a README"])
        let events = await rig.notifier.events
        #expect(events.contains { if case .jobSucceeded = $0 { true } else { false } })
    }

    @Test func unknownHealthFailsClosed() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.say("Hey Zora, add a README. Do it Zora.", at: -2)
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
        #expect(await rig.notifier.events.contains(.captureDiscarded(reason: .suppressionUnknown)))
    }

    @Test func segmentsDuringDictationAreTaggedAndIgnored() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.coordinator.handle(.began(rig.clock.now - 5))
        await rig.say("Hey Zora delete the repo. Do it Zora.", at: -4)
        await rig.coordinator.handle(.ended(rig.clock.now))
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
        #expect(await rig.store.segments.map(\.1) == [.dictation])
    }

    @Test func lateDictationEventIsCaughtByHoldBack() async {
        // The transcript arrives before CoreAudio reports the dictation that produced it.
        let rig = Rig(holdBack: 1.5)
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        let spokenAt = rig.clock.now - 3
        await rig.say("Hey Zora push to main. Do it Zora.", at: -3)
        #expect(await rig.coordinator.pendingCount == 1)
        await rig.coordinator.handle(.began(spokenAt - 0.2))
        await rig.coordinator.handle(.ended(rig.clock.now))
        rig.clock.advance(2)
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
        #expect(await rig.notifier.events.contains(.captureDiscarded(reason: .dictation)))
    }

    @Test func dictationStartAbortsOpenCapture() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.say("Hey Zora rename the module", at: -10)
        #expect(await rig.coordinator.isCapturing)
        await rig.coordinator.handle(.began(rig.clock.now))
        #expect(!(await rig.coordinator.isCapturing))
        await rig.coordinator.handle(.ended(rig.clock.now + 2))
        rig.clock.advance(5)
        await rig.say("do it Zora", at: 0)
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
    }

    @Test func dictationLongBeforeCaptureDoesNotBlock() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.coordinator.handle(.began(rig.clock.now - 60))
        await rig.coordinator.handle(.ended(rig.clock.now - 50))
        await rig.say("Hey Zora run tests. Do it Zora.", at: -2)
        await rig.settle()
        #expect(rig.harness.prompts == ["run tests"])
    }

    @Test func gapAbortsCapture() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.say("Hey Zora update deps", at: -3)
        await rig.coordinator.handle(.gap(reason: "device changed", at: rig.clock.now))
        await rig.say("do it Zora", at: 0)
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
        #expect(await rig.notifier.events.contains(.captureDiscarded(reason: .gap)))
    }

    @Test func healthLossNotifiesOnce() async {
        let rig = Rig()
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: false, reason: "x"))
        await rig.coordinator.handle(.health(healthy: false, reason: "x"))
        let degraded = await rig.notifier.events.filter { if case .degraded = $0 { true } else { false } }
        #expect(degraded.count == 1)
    }

    @Test func shutdownDropsPendingAndBlocksDispatch() async {
        let rig = Rig(holdBack: 1.5)
        await rig.coordinator.start()
        await rig.coordinator.handle(.health(healthy: true, reason: "ok"))
        await rig.say("Hey Zora run after shutdown. Do it Zora.", at: -2)
        #expect(await rig.coordinator.pendingCount == 1)
        await rig.coordinator.shutdown(timeout: 1)
        rig.clock.advance(5)
        await rig.settle()
        await rig.say("Hey Zora another. Do it Zora.", at: 0)
        rig.clock.advance(5)
        await rig.settle()
        #expect(rig.harness.prompts.isEmpty)
    }

    @Test func holdBackRequestsWakeInsteadOfTickingItself() async {
        final class Wakes: @unchecked Sendable { let lock = NSLock(); var delays: [Double] = [] }
        let wakes = Wakes()
        var c = ZoraConfig()
        c.capture.holdBackSeconds = 1.5
        let clock = FakeClock()
        let harness = RecordingHarness()
        let coordinator = Coordinator(
            config: c, store: MemoryStore(), notifier: NotifierHub(notifiers: [], routing: [:]), harness: harness,
            now: { clock.now }, scheduleWake: { d in wakes.lock.withLock { wakes.delays.append(d) } }
        )
        await coordinator.start()
        await coordinator.handle(.health(healthy: true, reason: "ok"))
        await coordinator.handle(.segment(TranscriptSegment(session: 1, start: clock.now - 2, end: clock.now - 1, text: "Hey Zora x. Do it Zora")))
        try? await Task.sleep(for: .seconds(2))
        // Nothing runs until the owner delivers the tick through its ordered queue.
        #expect(harness.prompts.isEmpty)
        #expect(wakes.lock.withLock { wakes.delays } == [1.5])
        clock.advance(2)
        await coordinator.tick()
        await coordinator.drainJobs()
        #expect(harness.prompts == ["x"])
    }
}
