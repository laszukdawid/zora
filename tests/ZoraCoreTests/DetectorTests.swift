import Foundation
import Testing
@testable import ZoraCore

private func detector(maxChars: Int = 4000) -> CommandDetector {
    var c = ZoraConfig()
    c.capture.maxChars = maxChars
    return CommandDetector(config: c.detectorConfig)
}

private func seg(_ text: String, _ start: Double, _ end: Double? = nil, session: UInt64 = 1) -> TranscriptSegment {
    TranscriptSegment(session: session, start: start, end: end ?? start + 1, text: text)
}

private func commands(_ outs: [DetectorOutput]) -> [String] {
    outs.compactMap { if case .command(let c) = $0 { return c.text } else { return nil } }
}

@Suite struct NormalizerTests {
    @Test func foldsPunctuationCaseAndDiacritics() {
        #expect(Normalizer.tokens("Hey, Zora!") == ["hey", "zora"])
        #expect(Normalizer.tokens("ZÓRA's  do-it") == ["zoras", "do", "it"])
        #expect(Normalizer.tokens("…") == [])
        #expect(Normalizer.tokens("Zora,fix") == ["zora", "fix"])
    }
}

@Suite struct DetectorTests {
    @Test func punctuatedPhrasesInOneSegment() {
        var d = detector()
        let out = d.feed(seg("Hey, Zora. Fix the login bug, please. Do it, Zora.", 0))
        #expect(commands(out) == ["Fix the login bug, please"])
        #expect(!d.isCapturing)
    }

    @Test func alternateEndPhraseAndCase() {
        var d = detector()
        #expect(commands(d.feed(seg("HEY ZORA run the tests. Zora, do it!", 0))) == ["run the tests"])
    }

    @Test func ambientSpeechNeverStartsCapture() {
        var d = detector()
        #expect(d.feed(seg("please refactor the service", 0)).isEmpty)
        #expect(d.feed(seg("do it Zora", 1)).isEmpty)
    }

    @Test func phrasesSplitAcrossSegments() {
        var d = detector()
        #expect(d.feed(seg("so I said hey", 0)).isEmpty)
        let started = d.feed(seg("Zora, check the logs", 1.2))
        #expect(started.contains { if case .captureStarted = $0 { true } else { false } })
        #expect(d.feed(seg("and summarize them. Do it", 3)).isEmpty)
        #expect(commands(d.feed(seg("Zora.", 4))) == ["check the logs and summarize them"])
    }

    @Test func staleIdleTailDoesNotJoin() {
        var d = detector()
        _ = d.feed(seg("hey", 0, 0.5))
        #expect(d.feed(seg("Zora check the logs", 10)).isEmpty)
        #expect(!d.isCapturing)
    }

    @Test func multipleCommandsInOneSegment() {
        var d = detector()
        let out = d.feed(seg("Hey Zora fix bug do it Zora. Hey Zora run tests, Zora do it", 0))
        #expect(commands(out) == ["fix bug", "run tests"])
    }

    @Test func cancelPhraseDiscards() {
        var d = detector()
        let out = d.feed(seg("Hey Zora delete everything. Zora, cancel. Do it Zora", 0))
        #expect(commands(out).isEmpty)
        #expect(out.contains(.discarded(.cancelled)))
    }

    @Test func emptyCommandIsDiscarded() {
        var d = detector()
        #expect(d.feed(seg("Hey Zora. Do it, Zora.", 0)).contains(.discarded(.empty)))
    }

    @Test func tooLongCaptureIsDiscarded() {
        var d = detector(maxChars: 40)
        _ = d.feed(seg("Hey Zora", 0))
        #expect(d.feed(seg(String(repeating: "word ", count: 20), 1)).contains(.discarded(.tooLong)))
        #expect(d.feed(seg("do it Zora", 2)).isEmpty)
    }

    @Test func tickExpiresCaptureInSilence() {
        var d = detector()
        _ = d.feed(seg("Hey Zora write a test", 0))
        #expect(d.tick(now: 20).isEmpty)
        #expect(d.tick(now: 32) == [.discarded(.gap)])
    }

    @Test func captureTimeoutEvenWithSpeech() {
        var d = detector()
        _ = d.feed(seg("Hey Zora start", 0))
        var t = 0.0
        while t < 175 { t += 20; _ = d.feed(seg("more words", t)) }
        #expect(d.tick(now: 181) == [.discarded(.timeout)])
    }

    @Test func largeGapBetweenSegmentsAbortsBeforeAppending() {
        var d = detector()
        _ = d.feed(seg("Hey Zora fix", 0))
        let out = d.feed(seg("do it Zora", 100))
        #expect(out == [.discarded(.gap)])
    }

    @Test func abortOnlyReportsWhenCapturing() {
        var d = detector()
        #expect(d.abort(.gap).isEmpty)
        _ = d.feed(seg("hey zora x", 0))
        #expect(d.abort(.dictation) == [.discarded(.dictation)])
    }

    @Test func captureStartTimeIsStartPhraseWord() {
        var d = detector()
        let out = d.feed(seg("one two hey zora three", 10, 15))
        #expect(out.contains(.captureStarted(at: 12)))
    }

    @Test func delimiterGluedToPromptKeepsPromptText() {
        var d = detector()
        #expect(commands(d.feed(seg("Hey Zora,fix tests. Do it Zora.", 0))) == ["fix tests"])
        #expect(commands(d.feed(seg("hey zora do-it-yourself guide please, zora do it", 5))) == ["do-it-yourself guide please"])
    }

    @Test func limitsApplyWhenBothDelimitersArriveTogether() {
        var d = detector(maxChars: 20)
        let long = d.feed(seg("Hey Zora " + String(repeating: "word ", count: 10) + "do it Zora", 0))
        #expect(long.contains(.discarded(.tooLong)) && commands(long).isEmpty)
        var d2 = detector()
        let slow = d2.feed(seg("Hey Zora write docs do it Zora", 0, 400))
        #expect(slow.contains(.discarded(.timeout)) && commands(slow).isEmpty)
    }
}
