import Foundation

/// Single ordering point for transcript, dictation and timer events. It is the only place that decides
/// whether a captured command runs.
public actor Coordinator {
    private let config: ZoraConfig
    private let store: any TranscriptStoring
    private let notifier: NotifierHub
    private let harness: any Harness
    private let now: @Sendable () -> HostSeconds
    private let scheduleWake: @Sendable (Double) -> Void
    private var accepting = true
    private var detector: CommandDetector
    private var policy: SuppressionPolicy
    private var dispatcher: Dispatcher?
    private var pending: [(command: Command, readyAt: HostSeconds)] = []
    private var dictationBeganAt: HostSeconds?
    private var lastMaintenance: HostSeconds
    private var lastStoreWarning: HostSeconds = -.infinity
    private var reportedUnhealthy = false

    public init(
        config: ZoraConfig, store: any TranscriptStoring, notifier: NotifierHub, harness: any Harness,
        now: @escaping @Sendable () -> HostSeconds = HostClock.now,
        scheduleWake: @escaping @Sendable (Double) -> Void = { _ in }
    ) {
        self.config = config
        self.store = store
        self.notifier = notifier
        self.harness = harness
        self.now = now
        self.scheduleWake = scheduleWake
        self.detector = CommandDetector(config: config.detectorConfig)
        self.policy = SuppressionPolicy(pad: config.dictation.padSeconds)
        self.lastMaintenance = now()
    }

    public func start() async {
        dispatcher = Dispatcher(
            timeout: config.jobs.timeoutSeconds, maxQueue: config.jobs.maxQueue, maxAge: config.jobs.maxAgeSeconds, now: now
        ) { [weak self] update in await self?.handle(update) }
        await notifier.notify(.started(dryRun: config.dryRun))
    }

    public var isCapturing: Bool { detector.isCapturing }
    public var pendingCount: Int { pending.count }

    // MARK: Transcript

    public func handle(_ event: TranscriptEvent) async {
        switch event {
        case .segment(let seg):
            let dictated = policy.overlapsDictation(seg.start, seg.end)
            // Update state before any await so a reentrant call never sees a half-applied event.
            let outputs = dictated ? detector.abort(.dictation) : detector.feed(seg)
            await apply(outputs)
            await store.append(seg, kind: dictated ? .dictation : .speech)
        case .gap:
            await apply(detector.abort(.gap))
        }
    }

    // MARK: Dictation

    public func handle(_ event: DictationEvent) async {
        switch event {
        case .began(let t):
            guard policy.apply(event) else { return }
            dictationBeganAt = t
            let outputs = detector.abort(.dictation)
            await apply(outputs)
            await store.retagDictation(from: wall(t - policy.pad), to: wall(now() + policy.pad))
        case .ended(let t):
            let began = dictationBeganAt ?? t
            policy.apply(event)
            dictationBeganAt = nil
            await store.retagDictation(from: wall(began - policy.pad), to: wall(t + policy.pad))
        case .health(let ok, let reason):
            policy.apply(event)
            if !ok && !reportedUnhealthy {
                reportedUnhealthy = true
                await notifier.notify(.degraded(reason: "dictation monitor: \(reason); commands paused"))
            } else if ok {
                reportedUnhealthy = false
            }
        }
    }

    // MARK: Timer

    /// Call about once a second, and when `scheduleWake` asks. Callers must deliver it through the same
    /// ordered queue as transcript and dictation events.
    public func tick() async {
        guard accepting else { return }
        let t = now()
        await apply(detector.tick(now: t))
        await releasePending(at: t)
        if t - lastMaintenance >= 60 {
            lastMaintenance = t
            await store.maintenance(retention: config.retentionHours * 3600)
            policy.prune(before: t - 600)
        } else {
            await store.flushIfDue()
        }
        if let failure = await store.takeFailure(), t - lastStoreWarning > 600 {
            lastStoreWarning = t
            await notifier.notify(.degraded(reason: failure))
        }
    }

    /// Stops accepting work, drops pending commands, cancels jobs and waits (bounded) for them to end.
    public func shutdown(timeout: Double = 8) async {
        accepting = false
        pending.removeAll()
        _ = detector.abort(.cancelled)
        if let dispatcher {
            await dispatcher.close()
            let drained = Task { await dispatcher.drain() }
            let deadline = Task { try? await Task.sleep(for: .seconds(timeout)); drained.cancel() }
            await drained.value
            deadline.cancel()
        }
        await store.flush()
    }

    public func drainJobs() async { await dispatcher?.drain() }

    public func flush() async { await store.flush() }

    // MARK: Internals

    private func apply(_ outputs: [DetectorOutput]) async {
        for output in outputs {
            switch output {
            case .captureStarted:
                await notifier.notify(.captureStarted)
            case .discarded(let reason):
                await notifier.notify(.captureDiscarded(reason: reason))
            case .command(let command):
                guard accepting else { continue }
                pending.append((command, now() + config.capture.holdBackSeconds))
                scheduleWake(config.capture.holdBackSeconds)
            }
        }
    }

    private func releasePending(at t: HostSeconds) async {
        let ready = pending.filter { $0.readyAt <= t }
        pending.removeAll { $0.readyAt <= t }
        for item in ready {
            switch policy.verdict(captureStart: item.command.captureStart, now: t) {
            case .dictation:
                await notifier.notify(.captureDiscarded(reason: .dictation))
            case .unknown:
                await notifier.notify(.captureDiscarded(reason: .suppressionUnknown))
            case .allow:
                await dispatch(item.command)
            }
        }
    }

    private func dispatch(_ command: Command) async {
        guard accepting, let dispatcher else { return }
        await store.recordCommand(id: command.id, text: command.text, harness: harness.id, status: "queued")
        // Shutdown may have started while the store write was suspended; the dispatcher rejects after close().
        guard accepting else {
            await store.finishCommand(id: command.id, status: "cancelled", exitCode: nil, summary: "shutting down")
            return
        }
        if await dispatcher.submit(id: command.id, prompt: command.text, harness: harness) {
            await notifier.notify(.commandQueued(id: command.id, text: command.text, harness: harness.id))
        } else {
            await store.finishCommand(id: command.id, status: "rejected", exitCode: nil, summary: "queue full")
            await notifier.notify(.captureDiscarded(reason: .queueFull))
        }
    }

    private func handle(_ update: JobUpdate) async {
        switch update {
        case .started(let id, let h):
            await store.finishCommand(id: id, status: "running", exitCode: nil, summary: "")
            await notifier.notify(.jobStarted(id: id, harness: h))
        case .finished(let id, _, let result):
            await store.finishCommand(id: id, status: result.success ? "succeeded" : "failed", exitCode: result.exitCode, summary: result.summary)
            await notifier.notify(result.success ? .jobSucceeded(id: id, summary: result.summary) : .jobFailed(id: id, reason: result.summary))
        case .expired(let id, _):
            await store.finishCommand(id: id, status: "expired", exitCode: nil, summary: "waited too long in queue")
            await notifier.notify(.captureDiscarded(reason: .expired))
        }
    }

    private func wall(_ host: HostSeconds) -> Date { HostClock.wall(for: host, now: now()) }
}
