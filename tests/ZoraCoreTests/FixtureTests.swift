import Foundation
import Testing
@testable import ZoraCore

/// Golden behaviour fixtures shared with the Rust core (crates/zora-core/tests/fixtures.rs).
@Suite struct FixtureTests {
    static let base = 1000.0

    static func cases() throws -> [[String: Any]] {
        let dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../../fixtures/behavior").standardized
        var all: [[String: Any]] = []
        for file in try FileManager.default.contentsOfDirectory(atPath: dir.path) where file.hasSuffix(".json") {
            let data = try Data(contentsOf: dir.appendingPathComponent(file))
            all += (try JSONSerialization.jsonObject(with: data) as? [[String: Any]]) ?? []
        }
        return all.filter { (($0["cores"] as? [String]) ?? ["swift"]).contains("swift") }
    }

    static func label(_ e: ZoraEvent) -> String? {
        switch e {
        case .captureStarted: "captureStarted"
        case .commandQueued(_, let text, _): "command:\(text)"
        case .captureDiscarded(let r): "discarded:\(r.rawValue)"
        case .degraded: "degraded"
        default: nil
        }
    }

    func run(_ c: [String: Any]) async -> (events: [String], kinds: [String]) {
        var config = ZoraConfig()
        config.capture.holdBackSeconds = c["holdBack"] as? Double ?? 0
        if let m = c["maxChars"] as? Int { config.capture.maxChars = m }
        let clock = FakeClock(Self.base)
        let store = MemoryStore()
        let notifier = RecordingNotifier()
        let coordinator = Coordinator(
            config: config, store: store, notifier: NotifierHub(notifiers: [notifier], routing: ["rec": ["*"]]),
            harness: RecordingHarness(), now: { clock.now }
        )
        await coordinator.start()
        func at(_ v: Any?) -> Double { Self.base + ((v as? NSNumber)?.doubleValue ?? 0) }
        var now = Self.base
        for step in c["steps"] as? [[String: Any]] ?? [] {
            let target = at(step["at"])
            if target > now { clock.advance(target - now); now = target }
            if let h = step["health"] as? Bool {
                await coordinator.handle(.health(healthy: h, reason: "fixture"))
            } else if step["began"] != nil {
                await coordinator.handle(.began(at(step["began"])))
            } else if step["ended"] != nil {
                await coordinator.handle(.ended(at(step["ended"])))
            } else if step["gap"] != nil {
                await coordinator.handle(.gap(reason: "fixture", at: now))
            } else if let s = step["say"] as? [Any] {
                await coordinator.handle(.segment(TranscriptSegment(session: 1, start: at(s[0]), end: at(s[1]), text: s[2] as? String ?? "")))
            }
            await coordinator.tick()
            await coordinator.drainJobs()
        }
        let end = now + config.capture.holdBackSeconds + 3
        while now < end {
            clock.advance(0.25); now += 0.25
            await coordinator.tick()
            await coordinator.drainJobs()
        }
        let events = await notifier.events.compactMap(Self.label)
        let kinds = await store.segments.map { $0.1.rawValue }
        return (events, kinds)
    }

    @Test func goldenFixtures() async throws {
        let cases = try Self.cases()
        #expect(cases.count >= 18)
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let (events, kinds) = await run(c)
            #expect(events == (c["expect"] as? [String] ?? []), "\(name)")
            if let want = c["kinds"] as? [String] { #expect(kinds == want, "\(name) kinds") }
        }
    }
}
