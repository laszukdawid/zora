import AppKit
import Foundation
import ZoraMac
import os

private let log = Logger(subsystem: "dev.zora.agent", category: "bridge")

/// The macOS plugin side of docs/protocol.md, in "attached" mode: this app process owns the microphone, speech,
/// dictation monitoring and notifications (so TCC sees the app's identity) and runs the Rust core `zorad` as a
/// child that talks the protocol over fds 3 (core reads) and 4 (core writes).
final class Bridge: @unchecked Sendable {
    struct Options {
        var zorad: String
        var coreArgs: [String]
        var audioFile: URL?
    }

    private let options: Options
    private let lock = NSLock()
    private var seq: UInt64 = 0 // only touched on `writer`
    private var generation: UInt64 = 0
    private var ready = false
    private var writeFD: Int32 = -1
    private var corePID: pid_t = 0
    private var stopping = false
    private var started = false
    private var pipeline: SpeechPipeline?
    private var monitor: CoreAudioDictationMonitor?
    private let notifier = MacNotifier()
    private let writer = DispatchQueue(label: "dev.zora.bridge.writer")
    private var keep: [AnyObject] = []
    /// Created on the main thread in `run()`.
    private var indicator: StatusIndicator?

    init(options: Options) { self.options = options }

    // MARK: Lifecycle

    private var cli: ZoradCLI {
        let args = options.coreArgs
        let config = args.firstIndex(of: "--config").flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
        return ZoradCLI(path: options.zorad, configPath: config)
    }

    @MainActor
    func startIndicator() {
        indicator = StatusIndicator(
            cli: cli,
            pauseMic: { [weak self] in self?.setLocalPause(true) },
            restoreMic: { [weak self] in self?.setLocalPause(false) },
            settingsSaved: { [weak self] in self?.recheck.signal() }
        )
    }

    func run() {
        // A windowless background listener must never be quit as "idle" by AppKit/macOS.
        ProcessInfo.processInfo.disableAutomaticTermination("Zora listens in the background")
        ProcessInfo.processInfo.disableSuddenTermination()
        // A core that closes its end must not kill the app: EPIPE is handled as connection loss instead.
        signal(SIGPIPE, SIG_IGN)
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            src.setEventHandler { [weak self] in self?.requestStop() }
            src.resume()
            keep.append(src)
        }
        let center = NSWorkspace.shared.notificationCenter
        keep.append(center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: nil) { [weak self] _ in
            self?.pipeline?.markDiscontinuity()
        } as AnyObject)
        Thread.detachNewThread { [self] in supervise() }
    }

    /// Orderly stop: the core sends `stop`, we finalize audio and answer `stopped`, then the core exits.
    func requestStop() {
        let pid: pid_t = lock.withLock { stopping = true; return corePID }
        if pid > 0 { kill(pid, SIGTERM) } else { exit(0) }
    }

    private func supervise() {
        var backoff = 1.0
        while true {
            let startedAt = Date()
            guard let pid = spawnCore() else {
                log.error("cannot start zorad at \(self.options.zorad, privacy: .public)")
                Thread.sleep(forTimeInterval: backoff)
                backoff = min(backoff * 2, 60)
                continue
            }
            var status: Int32 = 0
            while waitpid(pid, &status, 0) == -1 && errno == EINTR {}
            let (isStopping, fd): (Bool, Int32) = lock.withLock {
                ready = false
                corePID = 0
                let fd = writeFD
                writeFD = -1
                return (stopping, fd)
            }
            if fd >= 0 { close(fd) }
            if isStopping {
                Task { await self.pipeline?.stop(); exit(0) }
                Thread.sleep(forTimeInterval: 10)
                exit(0)
            }
            coreLost()
            let refusedConfig = (status & 0x7f) == 0 && ((status >> 8) & 0xff) == Self.exitConfig
            if refusedConfig {
                waitForValidConfig()
                backoff = 1
                continue
            }
            log.error("zorad exited (status \(status)); restarting")
            if Date().timeIntervalSince(startedAt) > 60 { backoff = 1 }
            Thread.sleep(forTimeInterval: backoff)
            backoff = min(backoff * 2, 60)
        }
    }

    /// `zorad run` exits with EX_CONFIG when the config is invalid.
    private static let exitConfig: Int32 = 78
    /// Signalled when Settings saved a change, so a fixed config starts the core at once.
    private let recheck = DispatchSemaphore(value: 0)

    /// The core refused the config. The app stays in the menu bar showing the problem (Settings can fix most
    /// problems) and starts the core again once `zorad settings` reports none.
    private func waitForValidConfig() {
        log.error("zorad refused the config; waiting for it to be fixed")
        let cli = self.cli
        while true {
            // Checked the way `zorad run` will check it, so a dry run doesn't wait for a project folder.
            let dryRun = options.coreArgs.contains("--dry-run")
            let problems = Self.blocking { () -> [String] in
                do { return try await cli.settings(dryRun: dryRun).problems } catch { return ["\(error)"] }
            }
            let stop = lock.withLock { stopping }
            if stop { exit(0) }
            if problems.isEmpty { break }
            Task { @MainActor [weak self] in self?.indicator?.setNeedsSetup(problems) }
            // Polling also catches fixes made outside Settings, such as a project folder that comes back.
            _ = recheck.wait(timeout: .now() + 5)
            while recheck.wait(timeout: .now()) == .success {}
        }
        log.info("config is valid again; starting the core")
        Task { @MainActor [weak self] in self?.indicator?.setNeedsSetup(nil) }
    }

    /// Runs async work from the supervisor thread.
    private static func blocking<T: Sendable>(_ work: @escaping @Sendable () async -> T) -> T {
        let done = DispatchSemaphore(value: 0)
        let box = OSAllocatedUnfairLock<T?>(initialState: nil)
        Task {
            let v = await work()
            box.withLock { $0 = v }
            done.signal()
        }
        done.wait()
        return box.withLock { $0! }
    }

    private func spawnCore() -> pid_t? {
        var toCore: [Int32] = [0, 0], fromCore: [Int32] = [0, 0]
        guard pipe(&toCore) == 0, pipe(&fromCore) == 0 else { return nil }
        // Move all four descriptors above 9 so dup2 onto 3 and 4 never collides with an existing number.
        func high(_ fd: Int32) -> Int32 { let n = fcntl(fd, F_DUPFD_CLOEXEC, 10); close(fd); return n }
        let coreRead = high(toCore[0]), ourWrite = high(toCore[1]), ourRead = high(fromCore[0]), coreWrite = high(fromCore[1])

        var actions = posix_spawn_file_actions_t(bitPattern: 0)
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addinherit_np(&actions, 1)
        posix_spawn_file_actions_addinherit_np(&actions, 2)
        posix_spawn_file_actions_adddup2(&actions, coreRead, 3)
        posix_spawn_file_actions_adddup2(&actions, coreWrite, 4)
        var attr = posix_spawnattr_t(bitPattern: 0)
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))
        var all = sigset_t(), none = sigset_t()
        sigfillset(&all); sigemptyset(&none)
        posix_spawnattr_setsigdefault(&attr, &all)
        posix_spawnattr_setsigmask(&attr, &none)

        let args = ["zorad", "run", "--attached-plugin"] + options.coreArgs
        let argv = args.map { strdup($0) } + [nil]
        defer { argv.forEach { free($0) } }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, options.zorad, &actions, &attr, argv, environ)
        close(coreRead); close(coreWrite)
        guard rc == 0 else { close(ourRead); close(ourWrite); return nil }
        let conn: UInt64 = lock.withLock {
            corePID = pid; writeFD = ourWrite; ready = false
            connection += 1
            return connection
        }
        Thread.detachNewThread { [self] in readLoop(fd: ourRead, pid: pid, conn: conn) }
        return pid
    }

    /// `conn` identifies this core connection: messages still buffered from a core that has gone are ignored.
    private func readLoop(fd: Int32, pid myPID: pid_t, conn: UInt64) {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n < 0 && errno == EINTR { continue }
            if n <= 0 { break }
            buffer.append(contentsOf: chunk[0..<n])
            while let nl = buffer.firstIndex(of: 0x0A) {
                let line = buffer[buffer.startIndex..<nl]
                buffer.removeSubrange(buffer.startIndex...nl)
                if let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any] { handle(obj, conn: conn) }
            }
            if buffer.count > 64 * 1024 { break } // protocol limit
        }
        close(fd)
        // A reader that stopped while its core still runs leaves a half-open channel: restart that core (never a
        // newer one).
        let pid: pid_t = lock.withLock { stopping || corePID != myPID ? 0 : myPID }
        if pid > 0 {
            log.error("protocol channel from core closed; restarting it")
            kill(pid, SIGKILL)
        }
    }

    // MARK: Protocol

    private func handle(_ msg: [String: Any], conn: UInt64) {
        // Lines still buffered from a core that has gone must not touch the mic or the new connection.
        guard lock.withLock({ conn == connection }) else { return }
        switch msg["t"] as? String {
        case "hello":
            let gen = (msg["generation"] as? NSNumber)?.uint64Value ?? 0
            guard (msg["clock"] as? String) == "darwin-uptime-raw-ns" else { log.error("clock domain mismatch"); return }
            let config = msg["config"] as? [String: Any] ?? [:]
            let isRestart: Bool = lock.withLock {
                let restart = generation != 0
                generation = gen
                coreEpoch = nil // epochs are per connection and start again with `start`
                lastMic = nil
                return restart
            }
            pendingConfig = config
            writer.sync { lock.withLock { announced = nil } } // after any mark queued for the old epochs
            sendRaw(["t": "ready", "protocol": 1, "generation": gen, "capabilities": ["transcript", "dictation", "notify", "listening"],
                     "clock": "darwin-uptime-raw-ns", "now_ns": HostClock.nowNanos()])
            lock.withLock { ready = true }
            if isRestart { pipeline?.markDiscontinuity() } // sessions are scoped to a generation
        case "start":
            listeningChanged(msg, conn: conn)
            startCapabilities()
        case "listening":
            listeningChanged(msg, conn: conn)
        case "sync":
            let id = msg["id"] as? NSNumber ?? 0
            monitor?.sync(reply: { [weak self] o in
                self?.send(["t": "synced", "id": id, "at_ns": o.atNanos, "healthy": o.healthy, "capturing": o.capturing])
            }, unknown: { [weak self] f, t in self?.send(["t": "unknown", "from_ns": f, "to_ns": t]) })
        case "notify":
            let id = msg["id"] as? NSNumber ?? 0
            let title = msg["title"] as? String ?? "Zora", body = msg["body"] as? String ?? ""
            let kind = msg["kind"] as? String ?? ""
            let sounds = (pendingConfig["sounds"] as? Bool) ?? true
            Task { @MainActor [weak self] in
                guard let indicator = self?.indicator else { return }
                indicator.soundsEnabled = sounds
                indicator.handle(kind: kind, title: title, body: body)
            }
            Task { [weak self] in
                guard let self else { return }
                let listeningID = "dev.zora.listening"
                let cancelled = kind == "captureDiscarded" && body == "cancelled"
                let ok: Bool
                if cancelled {
                    // "Stop it, Zora": forget quietly. Withdraw the listening notice; nothing else is shown.
                    notifier.withdraw(identifier: listeningID)
                    ok = true
                } else if kind == "jobStarted" {
                    // Only moves the menu-bar icon; the "got it" banner already said it is running.
                    ok = true
                } else {
                    ok = await notifier.post(title: title, body: body, identifier: kind == "captureStarted" ? listeningID : UUID().uuidString)
                }
                send(["t": "ack", "id": id, "ok": ok])
            }
        case "clock":
            send(["t": "clock", "id": msg["id"] as? NSNumber ?? 0, "now_ns": HostClock.nowNanos()])
        case "stop":
            onMic { [weak self] in
                guard let self else { return }
                micReady = false
                retryTask?.cancel()
                await pipeline?.stop()
                micRunning = false
                runningTag = nil
                writer.async { self.lock.withLock { self.announced = nil } }
                monitor?.stop()
                send(["t": "stopped"])
                // A later core connection starts fresh capture and a fresh monitoring snapshot.
                lock.withLock {
                    self.pipeline = nil
                    self.monitor = nil
                    self.started = false
                }
            }
        default:
            break
        }
    }

    private var pendingConfig: [String: Any] = [:]

    // MARK: Microphone on/off

    /// Start, pause, resume and stop of the microphone run one after another in this chain, in message order,
    /// so a late resume can never reopen the mic after a pause or a stop.
    private var micTail: Task<Void, Never>?
    // Under `lock`, and set the moment an event arrives (not when the chain gets to it), so a slow start-up
    // in the chain can never open the mic after a pause or a disconnect. The mic is open only while a core is
    // connected and says "listening", and no pause from the menu is waiting for the core to confirm it.
    /// nil while no core is connected (exited, restarting), so a pause saved meanwhile is never ignored.
    private var coreListening: Bool?
    private var coreUntil: Date?
    /// The core's listening epoch; nil for a core that sends none (it would reject `mic`) or none connected.
    private var coreEpoch: UInt64?
    private var localPause = false
    /// Counts core connections; see `readLoop`.
    private var connection: UInt64 = 0
    /// Bumped by every mic request. A request that isn't the newest when it reaches the head of the chain is
    /// dropped: the newer one behind it applies the latest state.
    private var micToken: UInt64 = 0
    /// The tag the core was last told is open; transcript output must carry it. Changed only on `writer`, in
    /// order with the `mic` messages.
    private var announced: CaptureTag?
    private var lastMic: (tag: CaptureTag, open: Bool)?
    // Touched only inside the chain.
    private var micReady = false
    private var micRunning = false
    private var runningTag: CaptureTag?
    private var micFailure: String?
    private var retryDelay = 1.0
    private var retryTask: Task<Void, Never>?

    private struct MicTarget { let tag: CaptureTag; let wanted: Bool; let epochs: Bool }

    private func onMic(_ work: @escaping @Sendable () async -> Void) {
        lock.withLock {
            let previous = micTail
            micTail = Task { await previous?.value; await work() }
        }
    }

    /// Queues a mic change that applies whatever state is current when it runs. The pipeline's grant changes
    /// now, so a pause or disconnect stops the engine from opening even before the chain gets to it.
    private func requestMic() {
        let token = updateGrant(bump: true)
        onMic { [weak self] in await self?.applyMic(token: token) }
    }

    @discardableResult
    private func updateGrant(bump: Bool) -> UInt64 {
        let (token, grant, pipeline): (UInt64, CaptureTag?, SpeechPipeline?) = lock.withLock {
            if bump { micToken += 1 }
            let wanted = coreListening == true && !localPause
            return (micToken, wanted ? CaptureTag(connection: connection, epoch: coreEpoch ?? 0) : nil, self.pipeline)
        }
        // Outside `lock`: the pipeline may hold its engine lock while calling back into the bridge.
        pipeline?.setGrant(grant, seq: token)
        return token
    }

    /// The state to apply; nil when `token` has been superseded.
    private func micTarget(token: UInt64?) -> MicTarget? {
        lock.withLock {
            if let token, token != micToken { return nil }
            return MicTarget(tag: CaptureTag(connection: connection, epoch: coreEpoch ?? 0),
                             wanted: coreListening == true && !localPause, epochs: coreEpoch != nil)
        }
    }

    private var micWanted: Bool { lock.withLock { coreListening == true && !localPause } }

    private func listeningChanged(_ msg: [String: Any], conn: UInt64) {
        let on = msg["listening"] as? Bool ?? true
        let until = (msg["until"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue) }
        let epoch = (msg["epoch"] as? NSNumber)?.uint64Value
        let confirmed = msg["confirmed"] as? Bool
        let current: Bool = lock.withLock {
            guard conn == connection else { return false }
            coreListening = on
            coreUntil = until
            coreEpoch = epoch
            if !on { localPause = false } // the core has it now
            return true
        }
        guard current else { return }
        log.info("listening \(on ? "on" : "paused", privacy: .public) epoch \(epoch.map(String.init) ?? "-", privacy: .public)")
        Task { @MainActor [weak self] in
            self?.indicator?.setListening(on, until: until, connection: conn, epoch: epoch, confirmed: confirmed)
        }
        // A confirmation repeats a state the chain already has.
        if confirmed == nil { requestMic() }
    }

    /// The menu's "Pause" closes the mic at once, without waiting for the core (which may be restarting), and
    /// keeps it closed until the core confirms; `false` withdraws it (the pause surely wasn't saved, or the user
    /// resumed).
    private func setLocalPause(_ on: Bool) {
        let core: (Bool?, Date?, UInt64, UInt64?) = lock.withLock {
            localPause = on
            return (coreListening, coreUntil, connection, coreEpoch)
        }
        // Withdrawn: the menu shows the core's state again (it may have shown a pause the core never got).
        if !on, let listening = core.0 {
            Task { @MainActor [weak self] in
                self?.indicator?.setListening(listening, until: core.1, connection: core.2, epoch: core.3, confirmed: nil)
            }
        }
        requestMic()
    }

    /// The core connection ended: close the mic until a new core says to listen.
    private func coreLost() {
        let gone: UInt64 = lock.withLock {
            coreListening = nil
            coreEpoch = nil
            connection += 1
            return connection
        }
        Task { @MainActor [weak self] in self?.indicator?.coreStopped(connection: gone) }
        requestMic()
    }

    /// Runs in the mic chain.
    private func applyMic(token: UInt64) async {
        guard let first = micTarget(token: token) else { return }
        guard micReady, let pipeline else { return reportMic(first) }
        if !first.wanted {
            retryTask?.cancel()
            retryDelay = 1
        }
        // Audio of an older epoch or connection is never carried over: close, then open under the new tag.
        if micRunning && (!first.wanted || runningTag != first.tag) {
            await pipeline.pauseMicrophone()
            micRunning = false
            runningTag = nil
        }
        // A newer request queued while pausing applies the rest (and reports).
        guard let target = micTarget(token: token) else { return }
        if target.wanted && !micRunning {
            do {
                try await pipeline.startMicrophone(tag: target.tag)
                micRunning = true
                runningTag = target.tag
                if micFailure != nil { log.info("microphone works again") }
                micFailure = nil
                retryDelay = 1
                retryTask?.cancel()
            } catch SpeechPipelineError.revoked {
                return // a newer request is queued and reports
            } catch {
                send(["t": "gap", "at_ns": HostClock.nowNanos(), "reason": "microphone start failed"])
                micFailed("\(error)", target: target)
            }
        }
        reportMic(target)
    }

    /// The pipeline's engine stopped on its own (a failed restart after a device change).
    private func micLost(tag: CaptureTag?, reason: String) {
        onMic { [weak self] in
            guard let self, micRunning, runningTag == tag, let pipeline else { return }
            await pipeline.pauseMicrophone() // stops the consumer as well
            micRunning = false
            runningTag = nil
            guard let target = micTarget(token: nil) else { return }
            micFailed(reason, target: target)
            reportMic(target)
        }
    }

    /// Runs in the mic chain. Reports a new reason once, and retries with backoff while listening is wanted.
    private func micFailed(_ reason: String, target: MicTarget) {
        log.error("microphone unavailable: \(reason, privacy: .public)")
        if micFailure != reason { send(["t": "status", "ready": false, "reason": "microphone unavailable: \(reason)"]) }
        micFailure = reason
        retryTask?.cancel()
        guard target.wanted else { return }
        let delay = retryDelay
        retryDelay = min(retryDelay * 2, 30)
        retryTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, let self, micWanted else { return }
            requestMic()
        }
    }

    /// Runs in the mic chain: tells the core (`mic`, only a core that sends epochs) and the menu what is open.
    private func reportMic(_ target: MicTarget) {
        let open = micRunning
        let tag = target.tag
        let tell: Bool = lock.withLock {
            guard tag.connection == connection, target.epochs,
                  lastMic.map({ $0.tag != tag || $0.open != open }) ?? true else { return false }
            lastMic = (tag, open)
            return true
        }
        // On `writer`, after `mic{open: true}` is written, so the core hears it before any output it allows.
        let mark = { [weak self] in
            guard let self else { return }
            lock.withLock { if tag.connection == self.connection { self.announced = open ? tag : nil } }
        }
        if tell {
            send(["t": "mic", "epoch": tag.epoch, "open": open], only: .connection(tag.connection), afterWrite: mark)
        } else {
            writer.async(execute: mark)
        }
        let failure = target.wanted ? micFailure : nil
        Task { @MainActor [weak self] in
            self?.indicator?.setMic(open: open, failure: failure, connection: tag.connection)
        }
    }

    /// Last capture state published by the monitor (began/ended); only touched on the monitor's queue.
    private var dictating = false
    private var pendingWrites = 0

    private func startCapabilities() {
        let first: Bool = lock.withLock { defer { started = true }; return !started }
        guard first else { return }
        let config = pendingConfig
        let bundleIDs = config["bundleIDs"] as? [String] ?? ["com.typewhisper.mac"]
        let monitor = CoreAudioDictationMonitor(bundleIDs: bundleIDs) { [weak self] event in
            switch event {
            case .began(let t):
                self?.dictating = true
                self?.send(["t": "dictation", "state": "began", "at_ns": HostClock.nanos(t)])
            case .ended(let t):
                self?.dictating = false
                self?.send(["t": "dictation", "state": "ended", "at_ns": HostClock.nanos(t)])
            case .health(let ok, let reason):
                self?.send(["t": "health", "healthy": ok, "capturing": self?.dictating ?? false, "reason": reason, "at_ns": HostClock.nowNanos()])
            }
        }
        monitor.start()
        monitor.startHeartbeat(heartbeat: { [weak self] o in
            self?.send(["t": "health", "healthy": o.healthy, "capturing": o.capturing, "reason": o.healthy ? "ok" : "unknown", "at_ns": o.atNanos])
        }, unknown: { [weak self] f, t in self?.send(["t": "unknown", "from_ns": f, "to_ns": t]) })
        self.monitor = monitor

        let locale = Locale(identifier: config["locale"] as? String ?? "en-US")
        let useDetector = config["speechDetector"] as? Bool ?? true
        let pipeline = SpeechPipeline(locale: locale, useDetector: useDetector) { [weak self] event in
            switch event {
            case .segment(let s):
                // File input has no tag and is never paused.
                self?.send(["t": "segment", "session": s.session, "start_ns": HostClock.nanos(s.start), "end_ns": HostClock.nanos(s.end), "text": s.text],
                           only: s.tag.map { .output($0) } ?? .any)
            case .gap(let reason, let at):
                self?.send(["t": "gap", "at_ns": HostClock.nanos(at), "reason": reason])
            }
        }
        if config["earlyWake"] as? Bool ?? true {
            pipeline.onPartial = { [weak self] session, text, tag in
                var msg: [String: Any] = ["t": "partial", "session": session, "text": String(text.suffix(512))]
                if let tag, tag.epoch > 0 { msg["epoch"] = tag.epoch }
                log.debug("partial chars=\(text.count)")
                self?.send(msg, only: tag.map { .output($0) } ?? .any)
            }
            log.info("early wake enabled")
        }
        pipeline.onMicLost = { [weak self] tag, reason in self?.micLost(tag: tag, reason: reason) }
        lock.withLock { self.pipeline = pipeline }
        updateGrant(bump: false)
        if let file = options.audioFile {
            // File mode is for testing and ignores pause.
            Task { [weak self] in
                do {
                    try await pipeline.prepare()
                    self?.send(["t": "status", "ready": true, "reason": "speech ready"])
                    try await pipeline.transcribeFile(file)
                    // Give hold-back and sync time, then stop the service.
                    try await Task.sleep(for: .seconds(5))
                    self?.requestStop()
                } catch {
                    self?.send(["t": "status", "ready": false, "reason": "\(error)"])
                }
            }
            return
        }
        onMic { [weak self] in
            guard let self else { return }
            do {
                try await pipeline.prepare()
                send(["t": "status", "ready": true, "reason": "speech ready"])
                // First run from launchd: the prompts belong to Zora.app itself, which is what TCC remembers.
                if Microphone.status == "not determined" { _ = await Microphone.request() }
                if notifier.isAvailable, await notifier.authorizationDescription() == "not determined" {
                    _ = await notifier.requestAuthorization()
                }
                guard Microphone.granted else { throw SpeechPipelineError.microphoneUnavailable("access is \(Microphone.status); allow Zora in System Settings → Privacy & Security → Microphone") }
                micReady = true
                await applyMic(token: lock.withLock { self.micToken })
            } catch {
                send(["t": "status", "ready": false, "reason": "\(error)"])
            }
        }
    }

    // MARK: Output

    /// The writer queue is the single point that assigns `seq` and writes, so sequence order is wire order.
    enum Only {
        case any
        /// Dropped if that connection has gone by the time it is written.
        case connection(UInt64)
        /// Transcript output: written only while its tag is the announced one on the current connection and epoch.
        case output(CaptureTag)
    }

    private func send(_ body: [String: Any], only: Only = .any, afterWrite: (() -> Void)? = nil) {
        let overloaded: Bool = lock.withLock {
            pendingWrites += 1
            return pendingWrites > 1024
        }
        if overloaded {
            // The core stopped reading. Dropping messages could hide a dictation event, so end the connection
            // instead; the supervisor restarts the core, which starts again from unknown suppression state.
            let pid: pid_t = lock.withLock { pendingWrites -= 1; return corePID }
            log.error("core is not reading; restarting it")
            if pid > 0 { kill(pid, SIGKILL) }
            return
        }
        writer.async { [self] in
            defer { lock.withLock { pendingWrites -= 1 } }
            guard let (fd, gen): (Int32, UInt64) = lock.withLock({
                let allowed = switch only {
                case .any: true
                case .connection(let c): c == connection
                case .output(let t): t == announced && t.connection == connection && t.epoch == (coreEpoch ?? 0)
                }
                return ready && writeFD >= 0 && allowed ? (writeFD, generation) : nil
            }) else { return }
            seq += 1
            var msg = body
            msg["seq"] = seq
            msg["generation"] = gen
            writeLine(msg, to: fd)
            afterWrite?()
        }
    }

    private func sendRaw(_ msg: [String: Any]) {
        writer.sync { [self] in
            guard let fd: Int32 = lock.withLock({ writeFD >= 0 ? writeFD : nil }) else { return }
            seq = 0
            writeLine(msg, to: fd)
        }
    }

    /// Runs on `writer`.
    private func writeLine(_ msg: [String: Any], to fd: Int32) {
        guard var data = try? JSONSerialization.data(withJSONObject: msg) else { return }
        data.append(0x0A)
        data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + off, raw.count - off)
                if n < 0 { if errno == EINTR { continue }; return }
                off += n
            }
        }
    }
}
