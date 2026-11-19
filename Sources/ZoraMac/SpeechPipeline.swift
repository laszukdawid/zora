import AVFAudio
import CoreMedia
import Foundation
import Speech
import os

private let log = Logger(subsystem: "dev.zora.agent", category: "speech")

public enum SpeechPipelineError: Error, CustomStringConvertible {
    case unsupportedLocale(String)
    case assetsMissing(String)
    case noAudioFormat
    case microphoneUnavailable(String)

    public var description: String {
        switch self {
        case .unsupportedLocale(let l): "speech transcription does not support locale \(l)"
        case .assetsMissing(let l): "speech model for \(l) is not installed; run `zora setup`"
        case .noAudioFormat: "no audio format compatible with the speech analyzer"
        case .microphoneUnavailable(let m): "microphone unavailable: \(m)"
        }
    }
}

/// Microphone (or file) → AVAudioConverter → SpeechAnalyzer(SpeechDetector + SpeechTranscriber) → final segments.
/// Timestamps are host seconds: each analyzer session is anchored to the host time of its first buffer.
public final class SpeechPipeline: @unchecked Sendable {
    struct Chunk { var buffer: AVAudioPCMBuffer; var host: HostSeconds; var wall: Date }

    private final class Session {
        let id: UInt64
        let h0: HostSeconds
        let analyzer: SpeechAnalyzer
        let input: AsyncStream<AnalyzerInput>.Continuation
        let results: Task<Void, Never>
        /// Cleared when the session's audio timeline is broken; late results are then discarded.
        let valid: OSAllocatedUnfairLock<Bool>
        init(id: UInt64, h0: HostSeconds, analyzer: SpeechAnalyzer, input: AsyncStream<AnalyzerInput>.Continuation,
             results: Task<Void, Never>, valid: OSAllocatedUnfairLock<Bool>) {
            self.id = id; self.h0 = h0; self.analyzer = analyzer; self.input = input; self.results = results; self.valid = valid
        }
    }

    private let requestedLocale: Locale
    private let useDetector: Bool
    private let onEvent: @Sendable (TranscriptEvent) -> Void
    private var locale: Locale?
    private var analyzerFormat: AVAudioFormat?

    private let engine = AVAudioEngine()
    private var chunks: AsyncStream<Chunk>.Continuation?
    private var consumer: Task<Void, Never>?
    private let dropped = OSAllocatedUnfairLock(initialState: false)
    private var configObserver: NSObjectProtocol?
    /// Serializes engine start, restart (device changes arrive on any thread), pause and stop.
    private let engineLock = NSLock()
    /// Under `engineLock`: the microphone should be open. A late device-change restart checks it.
    private var micOn = false
    /// Set while pausing: buffered audio is skipped and the open session is thrown away, not finalized.
    private let discarding = OSAllocatedUnfairLock(initialState: false)

    // Consumer-task state.
    private var session: Session?
    private var nextSessionID: UInt64 = UInt64(Date().timeIntervalSince1970 * 1000)
    private var converter: AVAudioConverter?
    private var lastEnd: (host: HostSeconds, wall: Date)?
    private var restartRequested = OSAllocatedUnfairLock(initialState: false)
    private var boundedInput = true

    /// Live, unfinished text (for early UI feedback only). Setting it before `prepare()` turns on volatile results.
    public var onPartial: (@Sendable (_ session: UInt64, _ text: String) -> Void)?

    public init(locale: Locale, useDetector: Bool, onEvent: @escaping @Sendable (TranscriptEvent) -> Void) {
        self.requestedLocale = locale
        self.useDetector = useDetector
        self.onEvent = onEvent
    }

    // MARK: Assets

    public static func assetStatus(for locale: Locale) async -> (Locale?, AssetInventory.Status) {
        guard let supported = await SpeechTranscriber.supportedLocale(equivalentTo: locale) else { return (nil, .unsupported) }
        let t = SpeechTranscriber(locale: supported, preset: .transcription)
        return (supported, await AssetInventory.status(forModules: [t]))
    }

    /// Downloads the on-device model if needed. Returns the resolved locale.
    public static func installAssets(for locale: Locale, progress: @escaping @Sendable (Double) -> Void) async throws -> Locale {
        guard let supported = await SpeechTranscriber.supportedLocale(equivalentTo: locale) else {
            throw SpeechPipelineError.unsupportedLocale(locale.identifier)
        }
        let t = SpeechTranscriber(locale: supported, preset: .transcription)
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [t]) {
            let observer = request.progress.observe(\.fractionCompleted) { p, _ in progress(p.fractionCompleted) }
            defer { observer.invalidate() }
            try await request.downloadAndInstall()
        }
        return supported
    }

    /// Resolves the locale and checks the model is installed. Never downloads.
    public func prepare() async throws {
        let (supported, status) = await Self.assetStatus(for: requestedLocale)
        guard let supported else { throw SpeechPipelineError.unsupportedLocale(requestedLocale.identifier) }
        if status != .installed {
            // A model already on disk (e.g. used by another app) only needs reserving for Zora, which is cheap.
            let onDisk = await SpeechTranscriber.installedLocales.contains { $0.identifier == supported.identifier }
            guard onDisk else { throw SpeechPipelineError.assetsMissing(supported.identifier) }
            _ = try await Self.installAssets(for: supported) { _ in }
        }
        locale = supported
        guard let fmt = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: makeModules().modules) else {
            throw SpeechPipelineError.noAudioFormat
        }
        analyzerFormat = fmt
        log.info("speech ready locale=\(supported.identifier, privacy: .public) format=\(fmt.sampleRate, privacy: .public)Hz")
    }

    // MARK: Microphone

    public func startMicrophone() throws {
        startConsumer()
        try engineLock.withLock {
            try installTapAndStart()
            micOn = true
        }
        if configObserver == nil {
            configObserver = NotificationCenter.default.addObserver(
                forName: .AVAudioEngineConfigurationChange, object: engine, queue: nil
            ) { [weak self] _ in self?.restartMicrophone(reason: "audio configuration changed") }
        }
    }

    /// Releases the microphone (the system's mic indicator goes off) and throws away what was heard but not yet
    /// transcribed, so nothing from before the pause arrives later. `startMicrophone()` resumes.
    public func pauseMicrophone() async {
        if let configObserver { NotificationCenter.default.removeObserver(configObserver) }
        configObserver = nil
        closeEngine()
        discarding.withLock { $0 = true }
        chunks?.finish()
        await consumer?.value
        chunks = nil
        consumer = nil
        lastEnd = nil
        restartRequested.withLock { $0 = false }
        dropped.withLock { $0 = false }
        discarding.withLock { $0 = false }
    }

    public func stop() async {
        if let configObserver { NotificationCenter.default.removeObserver(configObserver) }
        closeEngine()
        chunks?.finish()
        await consumer?.value
    }

    private func closeEngine() {
        engineLock.withLock {
            micOn = false
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
        }
    }

    /// Ends the current analyzer session and reports a gap (sleep/wake, device change).
    public func markDiscontinuity() {
        restartRequested.withLock { $0 = true }
    }

    private func restartMicrophone(reason: String) {
        engineLock.withLock {
            // A change notice that was already on its way when the mic was paused or stopped must not reopen it.
            guard micOn else { return }
            log.info("restarting microphone: \(reason, privacy: .public)")
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            markDiscontinuity()
            do { try installTapAndStart() } catch {
                micOn = false
                onEvent(.gap(reason: "microphone restart failed: \(error)", at: HostClock.now()))
            }
        }
    }

    private func installTapAndStart() throws {
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else {
            throw SpeechPipelineError.microphoneUnavailable("no input device")
        }
        // ~100 ms blocks keep wakeups low without adding much latency.
        let frames = AVAudioFrameCount(format.sampleRate / 10)
        input.installTap(onBus: 0, bufferSize: frames, format: format) { [weak self] buffer, when in
            guard let self, let copy = Self.copy(buffer) else { return }
            let host = when.isHostTimeValid ? HostClock.seconds(fromHostTime: when.hostTime) : HostClock.now()
            if case .dropped = self.chunks?.yield(Chunk(buffer: copy, host: host, wall: Date())) {
                self.dropped.withLock { $0 = true }
            }
        }
        engine.prepare()
        do { try engine.start() } catch {
            // Leave no tap behind: installing a second one on the same bus raises an exception.
            input.removeTap(onBus: 0)
            throw error
        }
    }

    // MARK: File (integration testing through the same path)

    public func transcribeFile(_ url: URL) async throws {
        startConsumer(bounded: false)
        let file = try AVAudioFile(forReading: url)
        let format = file.processingFormat
        let frames = AVAudioFrameCount(format.sampleRate / 10)
        let h0 = HostClock.now()
        var position: AVAudioFramePosition = 0
        let wall0 = Date()
        while position < file.length {
            guard let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { break }
            try file.read(into: buf, frameCount: frames)
            if buf.frameLength == 0 { break }
            let offset = Double(position) / format.sampleRate
            // Real-time pace, like a microphone: segment timestamps must never lie in the future.
            let wait = h0 + offset - HostClock.now()
            if wait > 0 { try await Task.sleep(for: .seconds(wait)) }
            chunks?.yield(Chunk(buffer: buf, host: h0 + offset, wall: wall0.addingTimeInterval(offset)))
            position += AVAudioFramePosition(buf.frameLength)
        }
        chunks?.finish()
        await consumer?.value
    }

    // MARK: Consumer

    private func startConsumer(bounded: Bool = true) {
        guard consumer == nil else { return }
        // Live: ~5 s of 100 ms chunks; overflow drops oldest and forces a gap. Files must not drop audio.
        boundedInput = bounded
        let (stream, cont) = AsyncStream<Chunk>.makeStream(bufferingPolicy: bounded ? .bufferingNewest(50) : .unbounded)
        chunks = cont
        consumer = Task.detached(priority: .utility) { [self] in
            for await chunk in stream { await self.process(chunk) }
            if self.discarding.withLock({ $0 }) { await self.discardSession() } else { await self.endSession() }
        }
    }

    private func process(_ chunk: Chunk) async {
        if discarding.withLock({ $0 }) { return }
        let lostAudio = dropped.withLock { v in defer { v = false }; return v }
        let restart = restartRequested.withLock { v in defer { v = false }; return v }
        var gapReason: String?
        if lostAudio { gapReason = "audio dropped" }
        if restart { gapReason = gapReason ?? "discontinuity" }
        if let last = lastEnd {
            if chunk.host - last.host > 0.5 { gapReason = gapReason ?? "audio clock jump" }
            if chunk.wall.timeIntervalSince(last.wall) > 2.5 { gapReason = gapReason ?? "wall clock jump (sleep?)" }
        }
        if let gapReason, session != nil {
            await endSession()
            onEvent(.gap(reason: gapReason, at: chunk.host))
        }
        let duration = Double(chunk.buffer.frameLength) / chunk.buffer.format.sampleRate
        lastEnd = (chunk.host + duration, chunk.wall.addingTimeInterval(duration))

        if session == nil {
            do { try await startSession(h0: chunk.host) } catch {
                onEvent(.gap(reason: "analyzer start failed: \(error)", at: chunk.host))
                return
            }
        }
        guard let session, let converted = convert(chunk.buffer) else { return }
        // Audio within a session is contiguous (any discontinuity ends the session above), so the analyzer's
        // own sample-count timeline is exact; explicit per-buffer times collide on resampling rounding.
        if case .dropped = session.input.yield(AnalyzerInput(buffer: converted)) {
            // The analyzer fell behind and lost samples, so its implicit timeline is now wrong. Throw the session
            // away without finalizing; nothing it produces from here on can be trusted.
            await discardSession()
            onEvent(.gap(reason: "speech analyzer overflow", at: chunk.host))
        }
    }

    private func makeModules() -> (modules: [any SpeechModule], transcriber: SpeechTranscriber) {
        let transcriber = SpeechTranscriber(
            locale: locale ?? requestedLocale, transcriptionOptions: [],
            reportingOptions: onPartial == nil ? [] : [.volatileResults, .fastResults], attributeOptions: []
        )
        var modules: [any SpeechModule] = [transcriber]
        if useDetector {
            modules.insert(SpeechDetector(detectionOptions: .init(sensitivityLevel: .medium), reportResults: false), at: 0)
        }
        return (modules, transcriber)
    }

    private func startSession(h0: HostSeconds) async throws {
        guard let format = analyzerFormat else { throw SpeechPipelineError.noAudioFormat }
        let (modules, transcriber) = makeModules()
        let analyzer = SpeechAnalyzer(modules: modules, options: .init(priority: .utility, modelRetention: .whileInUse))
        try await analyzer.prepareToAnalyze(in: format)
        let (input, cont) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: boundedInput ? .bufferingNewest(64) : .unbounded)
        nextSessionID += 1
        let id = nextSessionID
        let onEvent = self.onEvent
        let valid = OSAllocatedUnfairLock(initialState: true)
        let results = Task.detached(priority: .utility) { [weak self] in
            do {
                var lastPartial = ""
                var lastPartialAt = 0.0
                for try await r in transcriber.results {
                    let text = String(r.text.characters).trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !text.isEmpty, valid.withLock({ $0 }) else { continue }
                    if !r.isFinal {
                        // At most ~4 updates a second, and only when the text changed.
                        let now = HostClock.now()
                        if let onPartial = self?.onPartial, text != lastPartial, now - lastPartialAt >= 0.25 {
                            lastPartial = text
                            lastPartialAt = now
                            onPartial(id, text)
                        }
                        continue
                    }
                    lastPartial = ""
                    onEvent(.segment(TranscriptSegment(
                        session: id, start: h0 + r.range.start.seconds, end: h0 + r.range.end.seconds, text: text
                    )))
                }
            } catch {
                guard valid.withLock({ v in defer { v = false }; return v }) else { return }
                log.error("transcriber failed: \(String(describing: error), privacy: .public)")
                onEvent(.gap(reason: "transcriber error", at: HostClock.now()))
                self?.markDiscontinuity()
            }
        }
        try await analyzer.start(inputSequence: input)
        session = Session(id: id, h0: h0, analyzer: analyzer, input: cont, results: results, valid: valid)
        converter = nil
    }

    private func discardSession() async {
        guard let s = session else { return }
        session = nil
        s.valid.withLock { $0 = false }
        s.input.finish()
        await s.analyzer.cancelAndFinishNow()
        await s.results.value
    }

    private func endSession() async {
        guard let s = session else { return }
        session = nil
        s.input.finish()
        do { try await s.analyzer.finalizeAndFinishThroughEndOfInput() } catch {
            log.error("finalize failed: \(String(describing: error), privacy: .public)")
            await s.analyzer.cancelAndFinishNow()
        }
        await s.results.value
    }

    private func convert(_ buffer: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        guard let target = analyzerFormat else { return nil }
        if buffer.format == target { return buffer }
        if converter == nil || converter?.inputFormat != buffer.format {
            converter = AVAudioConverter(from: buffer.format, to: target)
            converter?.primeMethod = .none
        }
        guard let converter else { return nil }
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * target.sampleRate / buffer.format.sampleRate) + 32
        guard let out = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: capacity) else { return nil }
        var supplied = false
        var error: NSError?
        let status = converter.convert(to: out, error: &error) { _, inputStatus in
            if supplied { inputStatus.pointee = .noDataNow; return nil }
            supplied = true
            inputStatus.pointee = .haveData
            return buffer
        }
        guard status != .error, out.frameLength > 0 else { return nil }
        return out
    }

    private static func copy(_ src: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        guard let dst = AVAudioPCMBuffer(pcmFormat: src.format, frameCapacity: src.frameLength) else { return nil }
        dst.frameLength = src.frameLength
        let srcList = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: src.audioBufferList))
        let dstList = UnsafeMutableAudioBufferListPointer(dst.mutableAudioBufferList)
        for (s, d) in zip(srcList, dstList) {
            guard let sp = s.mData, let dp = d.mData else { continue }
            memcpy(dp, sp, Int(min(s.mDataByteSize, d.mDataByteSize)))
        }
        return dst
    }
}
