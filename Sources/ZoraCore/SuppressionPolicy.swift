import Foundation

/// Tracks push-to-talk dictation intervals and decides, fail-closed, whether speech may drive commands.
public struct SuppressionPolicy: Sendable {
    public var pad: Double
    public private(set) var healthy = false
    public private(set) var healthReason = "dictation monitor not started"
    private var openSince: HostSeconds?
    private var intervals: [ClosedRange<HostSeconds>] = []

    public init(pad: Double = 1.0) { self.pad = pad }

    public var isDictating: Bool { openSince != nil }

    /// Returns true when a dictation interval just began.
    @discardableResult
    public mutating func apply(_ event: DictationEvent) -> Bool {
        switch event {
        case .began(let t):
            guard openSince == nil else { return false }
            openSince = t
            return true
        case .ended(let t):
            guard let start = openSince else { return false }
            intervals.append(start...max(start, t))
            openSince = nil
        case .health(let ok, let reason):
            healthy = ok
            healthReason = reason
            // Losing track of state may hide an open interval; treat as still unknown until healthy again.
        }
        return false
    }

    /// Whether any dictation interval (padded) overlaps the given range.
    public func overlapsDictation(_ start: HostSeconds, _ end: HostSeconds) -> Bool {
        if let open = openSince, end >= open - pad { return true }
        return intervals.contains { start <= $0.upperBound + pad && end >= $0.lowerBound - pad }
    }

    public enum Verdict: Equatable, Sendable { case allow, dictation, unknown }

    /// Final check after the hold-back window: allow only if healthy and no dictation touched the capture.
    public func verdict(captureStart: HostSeconds, now: HostSeconds) -> Verdict {
        guard healthy else { return .unknown }
        return overlapsDictation(captureStart - 1, now) ? .dictation : .allow
    }

    public mutating func prune(before t: HostSeconds) {
        intervals.removeAll { $0.upperBound + pad < t }
    }
}
