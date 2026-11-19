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
}

public struct TranscriptSegment: Sendable, Equatable {
    public var session: UInt64
    public var start: HostSeconds
    public var end: HostSeconds
    public var text: String
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

public func expandTilde(_ path: String) -> String {
    (path as NSString).expandingTildeInPath
}
