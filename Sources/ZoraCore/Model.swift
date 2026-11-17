import Foundation

/// Monotonic seconds derived from `mach_absolute_time` (the clock CoreAudio and AVAudioTime use).
public typealias HostSeconds = Double

public enum HostClock {
    private static let secondsPerTick: Double = {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        return Double(info.numer) / Double(info.denom) / 1_000_000_000
    }()

    public static func now() -> HostSeconds { seconds(fromHostTime: mach_absolute_time()) }

    public static func seconds(fromHostTime ticks: UInt64) -> HostSeconds { Double(ticks) * secondsPerTick }

    /// Protocol clock domain `darwin-uptime-raw-ns` (same clock as `mach_absolute_time`, in nanoseconds).
    public static func nowNanos() -> Int64 { Int64(clock_gettime_nsec_np(CLOCK_UPTIME_RAW)) }

    public static func nanos(_ s: HostSeconds) -> Int64 { Int64((s * 1_000_000_000).rounded()) }

    /// Wall-clock time for a recent host time. Host time pauses during sleep, so only use it near `now`.
    public static func wall(for host: HostSeconds, now: HostSeconds = HostClock.now(), wallNow: Date = Date()) -> Date {
        wallNow.addingTimeInterval(host - now)
    }
}

public struct TranscriptSegment: Sendable, Equatable {
    public var session: UInt64
    public var start: HostSeconds
    public var end: HostSeconds
    public var text: String

    public init(session: UInt64, start: HostSeconds, end: HostSeconds, text: String) {
        self.session = session
        self.start = start
        self.end = end
        self.text = text
    }
}

public enum SegmentKind: String, Sendable {
    case speech
    case dictation
}

public enum TranscriptEvent: Sendable, Equatable {
    case segment(TranscriptSegment)
    /// Audio or recognition continuity was lost; any open capture must be abandoned.
    case gap(reason: String, at: HostSeconds)
}

public enum DictationEvent: Sendable, Equatable {
    case began(HostSeconds)
    case ended(HostSeconds)
    /// `healthy == false` means suppression state is unknown and commands must not run.
    case health(healthy: Bool, reason: String)
}

public struct Command: Sendable, Equatable {
    public var id: UUID
    public var text: String
    public var captureStart: HostSeconds
    public var captureEnd: HostSeconds

    public init(id: UUID = UUID(), text: String, captureStart: HostSeconds, captureEnd: HostSeconds) {
        self.id = id
        self.text = text
        self.captureStart = captureStart
        self.captureEnd = captureEnd
    }
}

public enum DiscardReason: String, Sendable, Equatable {
    case cancelled
    case timeout
    case gap
    case tooLong
    case empty
    case dictation
    case suppressionUnknown
    case queueFull
    case expired
}

public enum EventKind: String, Sendable, CaseIterable, Codable {
    case started, captureStarted, commandQueued, jobStarted, jobSucceeded, jobFailed, captureDiscarded, degraded
}

public enum ZoraEvent: Sendable, Equatable {
    case started(dryRun: Bool)
    case captureStarted
    case commandQueued(id: UUID, text: String, harness: String)
    case jobStarted(id: UUID, harness: String)
    case jobSucceeded(id: UUID, summary: String)
    case jobFailed(id: UUID, reason: String)
    case captureDiscarded(reason: DiscardReason)
    case degraded(reason: String)

    public var kind: EventKind {
        switch self {
        case .started: .started
        case .captureStarted: .captureStarted
        case .commandQueued: .commandQueued
        case .jobStarted: .jobStarted
        case .jobSucceeded: .jobSucceeded
        case .jobFailed: .jobFailed
        case .captureDiscarded: .captureDiscarded
        case .degraded: .degraded
        }
    }

    /// Human-facing title and body (may contain prompt/summary text; never logged).
    public var display: (title: String, body: String) {
        switch self {
        case .started(let dryRun): ("Zora", dryRun ? "Listening (dry run)" : "Listening")
        case .captureStarted: ("Zora", "Listening for a command…")
        case .commandQueued(_, let text, let harness): ("Zora → \(harness)", text)
        case .jobStarted(_, let harness): ("Zora", "Running on \(harness)")
        case .jobSucceeded(_, let summary): ("Zora: done", summary)
        case .jobFailed(_, let reason): ("Zora: failed", reason)
        case .captureDiscarded(let reason): ("Zora: command discarded", reason.rawValue)
        case .degraded(let reason): ("Zora: degraded", reason)
        }
    }
}
