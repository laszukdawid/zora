import AppKit

/// Menu-bar icon that always shows what Zora is doing, plus optional short sounds, so feedback does not depend
/// on how macOS presents notification banners (Focus, banner style). Its menu pauses and resumes listening and
/// opens Settings.
@MainActor
final class StatusIndicator: NSObject, NSMenuDelegate {
    enum State {
        case idle, listening, running, done, failed, degraded

        var symbol: String {
            switch self {
            case .idle: "waveform"
            case .listening: "mic.fill"
            case .running: "hourglass"
            case .done: "checkmark.circle.fill"
            case .failed: "exclamationmark.triangle.fill"
            case .degraded: "exclamationmark.circle"
            }
        }

        var label: String {
            switch self {
            case .idle: "Zora: listening for your wake phrase"
            case .listening: "Zora: heard the wake phrase, waiting for “Do it …”"
            case .running: "Zora: running your command…"
            case .done: "Zora: done"
            case .failed: "Zora: command failed or was discarded"
            case .degraded: "Zora: needs attention"
            }
        }

        var sound: String? {
            switch self {
            case .listening: "Tink"
            case .running: "Pop"
            case .done: "Glass"
            case .failed: "Basso"
            default: nil
            }
        }
    }

    private let cli: ZoradCLI
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let stateLine = NSMenuItem(title: State.idle.label, action: nil, keyEquivalent: "")
    private let pauseItem = NSMenuItem(title: "Pause Listening", action: nil, keyEquivalent: "")
    private let pauseHourItem = NSMenuItem(title: "Pause for 1 Hour", action: nil, keyEquivalent: "")
    private let historyAnchor = NSMenuItem.separator()
    private var history: [NSMenuItem] = []
    private let menu = NSMenu()
    private var resetTask: Task<Void, Never>?
    private var pendingTask: Task<Void, Never>?
    /// A pause or resume request is running; the items stay disabled until it finishes, so an older request's
    /// result can never undo a newer one.
    private var requestInFlight = false
    private var state = State.idle
    /// As last reported by the core; nil until it has said.
    private var listening: Bool?
    private var pausedUntil: Date?
    /// Shown instead of the state line until the core next reports its listening state.
    private var notice: String?
    /// The newest core connection the menu has heard about.
    private var connection: UInt64 = 0
    /// The menu's Pause was clicked and the core hasn't reported the pause yet.
    private var localPause = false
    /// Whether this app's microphone engine is open, as last reported by the mic chain.
    private var micOpen = false
    /// Why the microphone can't open while listening is wanted; nil when it works.
    private var micFailure: String?
    /// The core's listening epoch; nil for a core without epochs, which never confirms a pause.
    private var epoch: UInt64?
    /// The core's pause confirmation for `epoch` (docs/protocol.md "Confirmation").
    private var confirmed: Bool?
    private var confirmTimeout: Task<Void, Never>?
    /// Why the core won't start; nil while the config is valid.
    private var setupProblems: [String]?
    private var problemItems: [NSMenuItem] = []
    private let settingsItem = NSMenuItem(title: "Settings…", action: nil, keyEquivalent: ",")
    private lazy var settings = SettingsWindow(cli: cli, saved: settingsSaved)
    var soundsEnabled = true

    private let pauseMic: () -> Void
    private let restoreMic: () -> Void
    private let settingsSaved: () -> Void

    /// `pauseMic` releases the microphone right away when the user pauses from the menu; `restoreMic` ends
    /// that local pause (the pause could not be saved, or the user resumed) and shows the core's state again.
    /// `settingsSaved` runs after Settings saved a change.
    init(cli: ZoradCLI, pauseMic: @escaping () -> Void, restoreMic: @escaping () -> Void,
         settingsSaved: @escaping () -> Void) {
        self.cli = cli
        self.pauseMic = pauseMic
        self.restoreMic = restoreMic
        self.settingsSaved = settingsSaved
        super.init()
        stateLine.isEnabled = false
        menu.addItem(stateLine)
        menu.addItem(.separator())
        for (entry, action) in [(pauseItem, #selector(togglePause)), (pauseHourItem, #selector(pauseForHour))] {
            entry.target = self
            entry.action = action
            menu.addItem(entry)
        }
        settingsItem.action = #selector(openSettings)
        settingsItem.target = self
        menu.addItem(settingsItem)
        menu.addItem(historyAnchor)
        menu.addItem(.separator())
        let quit = NSMenuItem(title: "Restart Zora", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "")
        menu.addItem(quit)
        menu.delegate = self
        menu.autoenablesItems = false
        item.menu = menu
        setEnabled()
        render()
    }

    /// The core's listening state (`start` and `listening` messages, docs/protocol.md).
    /// `connection` numbers core connections (Bridge); updates queued from an older connection are ignored.
    /// `epoch` and `confirmed` come from the core's message; a call without `connection` is local and keeps them.
    func setListening(_ on: Bool, until: Date?, connection: UInt64? = nil, epoch: UInt64? = nil, confirmed: Bool? = nil) {
        if let connection {
            guard connection >= self.connection else { return }
            if epoch != self.epoch || connection != self.connection { self.confirmed = nil }
            self.connection = connection
            self.epoch = epoch
            if let confirmed { self.confirmed = confirmed }
            if !on { localPause = false }
            confirmTimeout?.cancel()
            if !on, epoch != nil, self.confirmed == nil {
                // The core answers within 5 s; a core that never does must not leave "Pausing…" up.
                confirmTimeout = Task { [weak self] in
                    try? await Task.sleep(for: .seconds(8))
                    guard !Task.isCancelled, let self, self.epoch == epoch, self.confirmed == nil else { return }
                    self.confirmed = false
                    render()
                }
            }
        }
        listening = on
        pausedUntil = on ? nil : until
        pendingTask?.cancel()
        pendingTask = nil
        notice = nil
        if !requestInFlight { setEnabled() }
        if !on {
            resetTask?.cancel()
            state = .idle
        }
        render()
    }

    /// The core is gone (exited, restarting): until a new one reports, the menu says it is starting rather than
    /// claiming to listen.
    func coreStopped(connection: UInt64) {
        guard connection >= self.connection else { return }
        self.connection = connection
        listening = nil
        pausedUntil = nil
        epoch = nil
        confirmed = nil
        confirmTimeout?.cancel()
        pendingTask?.cancel()
        pendingTask = nil
        resetTask?.cancel()
        notice = nil
        state = .idle
        setEnabled()
        render()
    }

    /// This app's microphone engine after the mic chain applied a change; `failure` says why it can't open
    /// while listening is wanted.
    func setMic(open: Bool, failure: String?, connection: UInt64) {
        guard connection >= self.connection else { return }
        micOpen = open
        micFailure = failure
        render()
    }

    /// The core refused the config (`problems`), or accepts it again (nil). While refused, the menu lists the
    /// problems and offers Settings; pausing makes no sense without a core.
    func setNeedsSetup(_ problems: [String]?) {
        guard problems != setupProblems else { return }
        let first = setupProblems == nil && problems != nil
        setupProblems = problems
        // Whatever was showing belongs to a core that has gone.
        pendingTask?.cancel()
        pendingTask = nil
        resetTask?.cancel()
        notice = nil
        state = .idle
        problemItems.forEach(menu.removeItem)
        problemItems = (problems ?? []).map { p in
            let entry = NSMenuItem(title: Self.friendly(p), action: nil, keyEquivalent: "")
            entry.toolTip = p
            entry.isEnabled = false
            return entry
        }
        for (i, entry) in problemItems.enumerated() { menu.insertItem(entry, at: menu.index(of: stateLine) + 1 + i) }
        setEnabled()
        render()
        // Shown once per episode, so a fresh install or a vanished project folder doesn't go unnoticed.
        if first, problems?.contains(where: { $0.contains("workspace") }) == true { settings.show() }
    }

    /// The menu has little room; the full text is in the tooltip.
    private static func friendly(_ problem: String) -> String {
        if problem.contains("workspace") { return "Choose a project folder in Settings" }
        return String(problem.prefix(80)) + (problem.count > 80 ? "…" : "")
    }

    /// Maps a core event kind (docs/protocol.md `notify.kind`) to a state.
    func handle(kind: String, title: String, body: String) {
        let next: State? = switch kind {
        case "captureStarted": .listening
        case "commandQueued", "jobStarted": .running
        case "jobSucceeded": .done
        case "jobFailed", "captureDiscarded": .failed
        case "degraded": .degraded
        default: nil
        }
        guard let next else { return }
        if kind == "captureDiscarded" && (body == "cancelled" || listening == false) {
            // Cancelled by voice, or cut off by a pause: back to idle, silently.
            resetTask?.cancel()
            set(.idle)
            return
        }
        set(next)
        if next != .listening { remember(title: title, body: body) }
        if soundsEnabled, let name = next.sound { NSSound(named: NSSound.Name(name))?.play() }
        // Finished states fall back to idle after a while; listening and running stay until the next event.
        resetTask?.cancel()
        // A capture ends within its 3-minute limit, so a "heard you" that nothing followed up on expires too.
        let after: Double? = switch next {
        case .done, .failed: 20
        case .degraded: 60
        case .listening: 200
        default: nil
        }
        if let after {
            resetTask = Task { [weak self] in
                try? await Task.sleep(for: .seconds(after))
                if !Task.isCancelled { self?.set(.idle) }
            }
        }
    }

    // MARK: Menu actions

    @objc private func togglePause() {
        change(listening == false ? ["resume"] : ["pause"])
    }

    @objc private func pauseForHour() {
        change(["pause", "1h"])
    }

    @objc private func openSettings() {
        settings.show()
    }

    /// Asks `zorad` for the change; the icon only moves when the core confirms it, so it never claims the mic
    /// is off while it is still on.
    private func change(_ args: [String]) {
        let wantListening = args.first == "resume"
        if !wantListening {
            localPause = true
            pauseMic()
            render()
        }
        pauseItem.isEnabled = false
        pauseHourItem.isEnabled = false
        requestInFlight = true
        Task {
            let result = await cli.run(args)
            requestInFlight = false
            guard result.ok else {
                if !wantListening && result.timedOut {
                    // It may or may not have been saved. The mic stays closed and the menu shows a pause, so
                    // Resume is offered and undoes it either way.
                    setListening(false, until: nil)
                    confirmed = false
                    notice = "Zora: paused here; the core didn't confirm it"
                    render()
                } else if !wantListening {
                    localPause = false
                    restoreMic() // surely not saved
                }
                setEnabled()
                alert(wantListening ? "Zora couldn't resume listening" : "Zora couldn't pause listening", result.message)
                return
            }
            if wantListening { // ends a pause held only here, if any
                localPause = false
                restoreMic()
            }
            if listening == wantListening { return setEnabled() } // the core already confirmed it
            // The core picks the change up within a second. If it doesn't answer (stopped or restarting), the
            // saved state still applies when it starts; say so instead of leaving the menu greyed out.
            pendingTask?.cancel()
            pendingTask = Task {
                try? await Task.sleep(for: .seconds(4))
                guard !Task.isCancelled else { return }
                setEnabled()
                notice = "Zora: change saved; it applies when Zora's core is running"
                render()
            }
        }
    }

    /// The items work only with a core that has said whether it listens, and never while a pause or resume
    /// is still running.
    private func setEnabled() {
        let on = setupProblems == nil && !requestInFlight && listening != nil
        pauseItem.isEnabled = on
        pauseHourItem.isEnabled = on
    }

    func menuWillOpen(_ menu: NSMenu) {
        render() // refreshes "paused until …"
    }

    // MARK: Display

    private func set(_ next: State) {
        state = next
        render()
    }

    private func render() {
        let paused = listening == false
        // Pausing until this app's engine is closed and, with a core that confirms, the core confirmed.
        let pausing = (localPause && listening == true) || (paused && (micOpen || (epoch != nil && confirmed == nil)))
        let symbol: String
        let label: String
        var detail: String?
        if setupProblems != nil {
            symbol = "gear.badge.xmark"
            label = "Zora needs setup: it isn't listening"
        } else if listening == nil && state == .idle {
            symbol = "ellipsis"
            label = "Zora: starting…"
        } else if paused && confirmed == false {
            // A timeout wins over Pausing…: the warning matters most while the microphone is still open.
            symbol = "exclamationmark.triangle"
            label = "Zora: pause not confirmed"
            detail = micOpen
                ? "Zora's microphone is still open."
                : "A transcript plugin didn't confirm that it closed its microphone."
        } else if pausing {
            symbol = "mic.badge.xmark"
            label = "Zora: pausing…"
        } else if !paused && micFailure != nil && state == .idle {
            symbol = "mic.slash.circle"
            label = "Zora: microphone unavailable"
            detail = micFailure
        } else if paused && state == .idle {
            symbol = "mic.slash"
            label = pausedUntil.map { "Zora: paused until \($0.formatted(date: .omitted, time: .shortened))" }
                ?? "Zora: paused (not listening)"
        } else {
            symbol = state.symbol
            label = state.label
        }
        let image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        image?.isTemplate = true
        item.button?.image = image
        item.button?.toolTip = detail.map { "\(label)\n\($0)" } ?? label
        stateLine.title = setupProblems == nil ? (notice ?? label) : label
        stateLine.toolTip = detail
        pauseItem.title = paused ? "Resume Listening" : "Pause Listening"
        pauseHourItem.isHidden = paused
    }

    private func remember(title: String, body: String) {
        let text = "\(title): \(body.replacingOccurrences(of: "\n", with: " "))"
        let entry = NSMenuItem(title: String(text.prefix(90)) + (text.count > 90 ? "…" : ""), action: nil, keyEquivalent: "")
        entry.toolTip = String(body.prefix(1000))
        entry.isEnabled = false
        history.insert(entry, at: 0)
        menu.insertItem(entry, at: menu.index(of: historyAnchor) + 1)
        if history.count > 5, let old = history.popLast() { menu.removeItem(old) }
    }

    private func alert(_ title: String, _ text: String) {
        NSApp.activate()
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        a.alertStyle = .warning
        a.runModal()
    }
}
