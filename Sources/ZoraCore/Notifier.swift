import Foundation
import os

public protocol Notifier: Sendable {
    var id: String { get }
    func notify(_ event: ZoraEvent) async
}

/// Routes events to notifiers by event kind, as configured.
public struct NotifierHub: Sendable {
    private let routes: [(notifier: any Notifier, kinds: Set<EventKind>)]

    public init(notifiers: [any Notifier], routing: [String: [String]]) {
        routes = notifiers.compactMap { n in
            guard let names = routing[n.id] else { return nil }
            let kinds = names.contains("*") ? Set(EventKind.allCases) : Set(names.compactMap(EventKind.init(rawValue:)))
            return (n, kinds)
        }
    }

    public func notify(_ event: ZoraEvent) async {
        for route in routes where route.kinds.contains(event.kind) {
            await route.notifier.notify(event)
        }
    }
}

/// Logs event metadata only; transcript and prompt text never reach the log.
public struct LogNotifier: Notifier {
    public let id = "log"
    private let logger = Logger(subsystem: "dev.zora.agent", category: "events")
    private let echo: Bool

    public init(echoToStderr: Bool = false) { self.echo = echoToStderr }

    public func notify(_ event: ZoraEvent) async {
        let line = Self.metadata(event)
        logger.info("\(line, privacy: .public)")
        if echo { FileHandle.standardError.write(Data("[zora] \(line)\n".utf8)) }
    }

    public static func metadata(_ event: ZoraEvent) -> String {
        switch event {
        case .started(let dry): "started dryRun=\(dry)"
        case .captureStarted: "captureStarted"
        case .commandQueued(let id, let text, let h): "commandQueued id=\(id) harness=\(h) chars=\(text.count)"
        case .jobStarted(let id, let h): "jobStarted id=\(id) harness=\(h)"
        case .jobSucceeded(let id, let s): "jobSucceeded id=\(id) summaryChars=\(s.count)"
        case .jobFailed(let id, _): "jobFailed id=\(id)"
        case .captureDiscarded(let r): "captureDiscarded reason=\(r.rawValue)"
        case .degraded(let r): "degraded reason=\(r)"
        }
    }
}
