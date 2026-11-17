import Foundation

public enum JobUpdate: Sendable, Equatable {
    case started(id: UUID, harness: String)
    case finished(id: UUID, harness: String, result: HarnessResult)
    case expired(id: UUID, harness: String)
}

/// Runs commands on harnesses: bounded FIFO queue, one job per workspace at a time, per-job timeout, max queue age.
public actor Dispatcher {
    struct Job { var id: UUID; var prompt: String; var harness: any Harness; var queuedAt: HostSeconds }

    private let timeout: Double
    private let maxQueue: Int
    private let maxAge: Double
    private let now: @Sendable () -> HostSeconds
    private let onUpdate: @Sendable (JobUpdate) async -> Void
    private var queue: [Job] = []
    private var busyWorkspaces: Set<String> = []
    private var running: [UUID: Task<Void, Never>] = [:]
    private var closed = false

    public init(
        timeout: Double, maxQueue: Int, maxAge: Double,
        now: @escaping @Sendable () -> HostSeconds = HostClock.now,
        onUpdate: @escaping @Sendable (JobUpdate) async -> Void
    ) {
        self.timeout = timeout
        self.maxQueue = maxQueue
        self.maxAge = maxAge
        self.now = now
        self.onUpdate = onUpdate
    }

    /// Returns false when the queue is full or the dispatcher is closed.
    public func submit(id: UUID, prompt: String, harness: any Harness) -> Bool {
        guard !closed, queue.count < maxQueue else { return false }
        queue.append(Job(id: id, prompt: prompt, harness: harness, queuedAt: now()))
        pump()
        return true
    }

    public var queuedCount: Int { queue.count }
    public var runningCount: Int { running.count }

    public func cancelAll() {
        queue.removeAll()
        running.values.forEach { $0.cancel() }
    }

    /// Permanently rejects new submissions and cancels everything in flight.
    public func close() {
        closed = true
        cancelAll()
    }

    /// Waits until nothing is queued or running (used by tests and one-shot CLI modes).
    public func drain() async {
        while !queue.isEmpty || !running.isEmpty {
            if Task.isCancelled { return }
            try? await Task.sleep(for: .milliseconds(20))
        }
    }

    private func pump() {
        var i = 0
        while i < queue.count {
            let job = queue[i]
            if now() - job.queuedAt > maxAge {
                queue.remove(at: i)
                let update = onUpdate
                Task { await update(.expired(id: job.id, harness: job.harness.id)) }
                continue
            }
            guard !busyWorkspaces.contains(job.harness.workspace) else { i += 1; continue }
            queue.remove(at: i)
            start(job)
        }
    }

    private func start(_ job: Job) {
        busyWorkspaces.insert(job.harness.workspace)
        let timeout = self.timeout
        let update = onUpdate
        running[job.id] = Task {
            await update(.started(id: job.id, harness: job.harness.id))
            let result = await job.harness.run(prompt: job.prompt, timeout: timeout)
            await update(.finished(id: job.id, harness: job.harness.id, result: result))
            self.finish(job)
        }
    }

    private func finish(_ job: Job) {
        busyWorkspaces.remove(job.harness.workspace)
        running[job.id] = nil
        pump()
    }
}
