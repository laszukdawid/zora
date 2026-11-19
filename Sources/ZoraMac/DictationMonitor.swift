import CoreAudio
import Foundation
import os

private let log = Logger(subsystem: "dev.zora.agent", category: "dictation")

/// Watches CoreAudio process objects and reports when any configured dictation app is capturing input.
/// Event times are when the callback was received; the coordinator's padding and hold-back absorb latency.
/// Health is reported healthy only when every listener is installed and every read succeeded; on any failure
/// it stays unknown (commands paused) and retries every few seconds.
public final class CoreAudioDictationMonitor: @unchecked Sendable {
    private let bundleIDs: Set<String>
    private let onEvent: @Sendable (DictationEvent) -> Void
    private let queue = DispatchQueue(label: "dev.zora.dictation")
    private var watched: [AudioObjectID: AudioObjectPropertyListenerBlock] = [:]
    private var capturing = false
    private var healthy: Bool?
    private var listListener: AudioObjectPropertyListenerBlock?
    private var retryScheduled = false
    private var stopped = false
    private var lastConfirmed: Int64?
    private var timer: DispatchSourceTimer?
    private var onUnknown: (@Sendable (Int64, Int64) -> Void)?

    public init(bundleIDs: [String], onEvent: @escaping @Sendable (DictationEvent) -> Void) {
        self.bundleIDs = Set(bundleIDs)
        self.onEvent = onEvent
    }

    public func start() {
        queue.async { [self] in _ = refresh() }
    }

    public struct Observation: Sendable {
        public var atNanos: Int64
        public var healthy: Bool
        public var capturing: Bool
    }

    /// Protocol-level monitoring (docs/protocol.md): a 1 s timer on the same serial queue as the CoreAudio
    /// callbacks takes a complete observation; a late timer means the loop stalled, which is reported as an
    /// unknown interval before anything else. Every second tick also sends a heartbeat.
    public func startHeartbeat(
        heartbeat: @escaping @Sendable (Observation) -> Void,
        unknown: @escaping @Sendable (_ fromNanos: Int64, _ toNanos: Int64) -> Void
    ) {
        queue.async { [self] in
            onUnknown = unknown
            let t = DispatchSource.makeTimerSource(queue: queue)
            var tick = 0
            t.schedule(deadline: .now() + 1, repeating: 1, leeway: .milliseconds(100))
            t.setEventHandler { [weak self] in
                guard let self else { return }
                let obs = self.refresh()
                tick += 1
                if tick % 2 == 0 { heartbeat(obs) }
            }
            t.resume()
            timer = t
        }
    }

    /// The barrier before a command runs: stall check, then a fresh complete snapshot, answered from the loop.
    public func sync(
        reply: @escaping @Sendable (Observation) -> Void,
        unknown: @escaping @Sendable (_ fromNanos: Int64, _ toNanos: Int64) -> Void
    ) {
        queue.async { [self] in
            if onUnknown == nil { onUnknown = unknown }
            reply(refresh())
        }
    }

    public func stop() {
        queue.sync {
            stopped = true
            timer?.cancel()
            timer = nil
            if let listListener {
                var addr = Self.address(kAudioHardwarePropertyProcessObjectList)
                AudioObjectRemovePropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &addr, queue, listListener)
            }
            for (id, block) in watched {
                var addr = Self.address(kAudioProcessPropertyIsRunningInput)
                AudioObjectRemovePropertyListenerBlock(id, &addr, queue, block)
            }
            watched.removeAll()
        }
    }

    /// One-off snapshot for `zora status`: bundle ID and whether it is capturing input, for watched apps.
    public static func snapshot(bundleIDs: [String]) -> [(bundleID: String, pid: pid_t, capturing: Bool)]? {
        guard let ids = processObjects() else { return nil }
        let wanted = Set(bundleIDs)
        return ids.compactMap { id in
            guard let lookup = bundleID(of: id), let bid = lookup, wanted.contains(bid) else { return nil }
            let pid: pid_t = read(id, kAudioProcessPropertyPID) ?? -1
            return (bid, pid, (read(id, kAudioProcessPropertyIsRunningInput) as UInt32?).map { $0 != 0 } ?? false)
        }
    }

    // MARK: Internals (on `queue`)

    private static let stallLimit: Int64 = 1_500_000_000

    /// The only path that observes and publishes state (callbacks, timer, retries and sync all use it).
    /// Order: build a complete snapshot without publishing, report the whole gap since the previous completed
    /// observation as unknown if it exceeds the limit, then publish transitions and health, then advance
    /// `lastConfirmed`.
    @discardableResult
    private func refresh() -> Observation {
        guard !stopped else { return Observation(atNanos: HostClock.nowNanos(), healthy: false, capturing: capturing) }
        let begin = HostClock.nowNanos()
        let result = snapshot()
        let end = HostClock.nowNanos()
        // The observation gap is from the previous completed observation to this one's completion; anything
        // longer than the limit (a stalled loop, a slow snapshot, or both) is reported whole, before publishing.
        let since = lastConfirmed ?? begin
        if end - since > Self.stallLimit {
            onUnknown?(since, end)
        }
        switch result {
        case .success(let nowCapturing):
            // Compare against the previous complete snapshot; an app that exits mid-capture ends the interval.
            let t = HostClock.now()
            if !capturing && nowCapturing { onEvent(.began(t)) }
            if capturing && !nowCapturing { onEvent(.ended(t)) }
            capturing = nowCapturing
            setHealth(true, "ok")
            lastConfirmed = end
        case .failure(let problem):
            setHealth(false, problem.message)
            lastConfirmed = nil
            scheduleRetry()
        }
        return Observation(atNanos: end, healthy: healthy == true, capturing: capturing)
    }

    struct Problem: Error { let message: String }

    /// Installs missing listeners and reads every watched process. Publishes nothing.
    private func snapshot() -> Result<Bool, Problem> {
        func fail(_ m: String) -> Result<Bool, Problem> { .failure(Problem(message: m)) }
        if listListener == nil {
            let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in self?.refresh() }
            var addr = Self.address(kAudioHardwarePropertyProcessObjectList)
            let status = AudioObjectAddPropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &addr, queue, block)
            guard status == noErr else { return fail("process list listener failed (\(status))") }
            listListener = block
        }
        guard let ids = Self.processObjects() else { return fail("cannot read audio process list") }
        let current = Set(ids)
        for gone in watched.keys where !current.contains(gone) { watched[gone] = nil }
        for id in ids where watched[id] == nil {
            // A read error is not "some other app": it could be the dictation app, so fail closed and retry.
            guard let lookup = Self.bundleID(of: id) else { return fail("cannot read bundle id of audio process \(id)") }
            guard let bid = lookup, bundleIDs.contains(bid) else { continue }
            let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in self?.refresh() }
            var addr = Self.address(kAudioProcessPropertyIsRunningInput)
            let status = AudioObjectAddPropertyListenerBlock(id, &addr, queue, block)
            guard status == noErr else { return fail("input listener failed for \(bid) (\(status))") }
            watched[id] = block
            log.info("watching \(bid, privacy: .public) object=\(id)")
        }
        var nowCapturing = false
        for id in watched.keys {
            guard let running: UInt32 = Self.read(id, kAudioProcessPropertyIsRunningInput) else {
                return fail("cannot read input state of watched process \(id)")
            }
            if running != 0 { nowCapturing = true }
        }
        return .success(nowCapturing)
    }

    private func scheduleRetry() {
        guard !retryScheduled else { return }
        retryScheduled = true
        queue.asyncAfter(deadline: .now() + 5) { [weak self] in
            self?.retryScheduled = false
            _ = self?.refresh()
        }
    }

    private func setHealth(_ ok: Bool, _ reason: String) {
        guard healthy != ok else { return }
        healthy = ok
        if !ok { log.error("unhealthy: \(reason, privacy: .public)") }
        onEvent(.health(healthy: ok, reason: reason))
    }

    // MARK: CoreAudio helpers

    private static func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    }

    private static func processObjects() -> [AudioObjectID]? {
        var addr = address(kAudioHardwarePropertyProcessObjectList)
        var size: UInt32 = 0
        let system = AudioObjectID(kAudioObjectSystemObject)
        guard AudioObjectGetPropertyDataSize(system, &addr, 0, nil, &size) == noErr else { return nil }
        var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(system, &addr, 0, nil, &size, &ids) == noErr else { return nil }
        return Array(ids.prefix(Int(size) / MemoryLayout<AudioObjectID>.size))
    }

    private static func read<T: FixedWidthInteger>(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector) -> T? {
        var addr = address(selector)
        var value: T = 0
        var size = UInt32(MemoryLayout<T>.size)
        return AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr ? value : nil
    }

    /// nil = the read failed; .some(nil) = the process has no bundle ID.
    private static func bundleID(of id: AudioObjectID) -> String?? {
        var addr = address(kAudioProcessPropertyBundleID)
        var value: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr else { return nil }
        guard let value else { return .some(nil) }
        let s = value.takeRetainedValue() as String
        return .some(s.isEmpty ? nil : s)
    }
}
