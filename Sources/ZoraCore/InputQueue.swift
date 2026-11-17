import Foundation

/// Ordered input to the coordinator: one consumer loop keeps transcript, dictation and timer events in order.
public enum Input: Sendable {
    case transcript(TranscriptEvent)
    case dictation(DictationEvent)
    case tick
    /// Transcript events were dropped here because the consumer fell behind.
    case lossMarker
}

/// Ordered, unbounded-for-control queue. Dictation events are never dropped; transcript events and ticks are
/// capped, and a dropped transcript event is replaced by a gap so no capture spans lost text.
public final class InputQueue: @unchecked Sendable {
    public let stream: AsyncStream<Input>
    private let sink: AsyncStream<Input>.Continuation
    private let lock = NSLock()
    private var bulk = 0
    private var lastWasLossMarker = false
    private let limit: Int

    public init(limit: Int = 1024) {
        (stream, sink) = AsyncStream<Input>.makeStream()
        self.limit = limit
    }

    public func send(_ input: Input) {
        // Yield under the lock so the loss marker lands exactly where the dropped event would have been.
        lock.withLock {
            switch input {
            case .dictation:
                sink.yield(input)
                lastWasLossMarker = false
            case .transcript, .tick:
                if bulk < limit {
                    bulk += 1
                    sink.yield(input)
                    lastWasLossMarker = false
                } else if case .transcript = input, !lastWasLossMarker {
                    // Uncounted; at most one per accepted event, so the stream stays bounded.
                    sink.yield(.lossMarker)
                    lastWasLossMarker = true
                }
            case .lossMarker:
                break
            }
        }
    }

    /// Called by the consumer for each item it takes.
    public func consumed(_ input: Input) {
        lock.withLock {
            switch input {
            case .transcript, .tick: bulk -= 1
            case .dictation, .lossMarker: break
            }
        }
    }

    public func finish() { sink.finish() }
}
