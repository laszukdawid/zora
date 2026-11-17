import AppKit

/// Menu-bar icon that always shows what Zora is doing, plus optional short sounds, so feedback does not depend
/// on how macOS presents notification banners (Focus, banner style).
@MainActor
final class StatusIndicator {
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

    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let stateLine = NSMenuItem(title: State.idle.label, action: nil, keyEquivalent: "")
    private var history: [NSMenuItem] = []
    private let menu = NSMenu()
    private var resetTask: Task<Void, Never>?
    var soundsEnabled = true

    init() {
        stateLine.isEnabled = false
        menu.addItem(stateLine)
        menu.addItem(.separator())
        let quit = NSMenuItem(title: "Restart Zora", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "")
        menu.addItem(quit)
        item.menu = menu
        set(.idle)
    }

    /// Maps a core event kind (docs/protocol.md `notify.kind`) to a state.
    func handle(kind: String, title: String, body: String) {
        let state: State? = switch kind {
        case "captureStarted": .listening
        case "commandQueued", "jobStarted": .running
        case "jobSucceeded": .done
        case "jobFailed", "captureDiscarded": .failed
        case "degraded": .degraded
        default: nil
        }
        guard let state else { return }
        if kind == "captureDiscarded" && body == "cancelled" {
            // Cancelled by voice: back to idle, silently.
            resetTask?.cancel()
            set(.idle)
            return
        }
        set(state)
        if state != .listening { remember(title: title, body: body) }
        if soundsEnabled, let name = state.sound { NSSound(named: NSSound.Name(name))?.play() }
        // Finished states fall back to idle after a while; listening and running stay until the next event.
        resetTask?.cancel()
        if [.done, .failed, .degraded].contains(state) {
            resetTask = Task { [weak self] in
                try? await Task.sleep(for: .seconds(state == .degraded ? 60 : 20))
                if !Task.isCancelled { self?.set(.idle) }
            }
        }
    }

    private func set(_ state: State) {
        let image = NSImage(systemSymbolName: state.symbol, accessibilityDescription: state.label)
        image?.isTemplate = true
        item.button?.image = image
        item.button?.toolTip = state.label
        stateLine.title = state.label
    }

    private func remember(title: String, body: String) {
        let text = "\(title): \(body.replacingOccurrences(of: "\n", with: " "))"
        let entry = NSMenuItem(title: String(text.prefix(90)) + (text.count > 90 ? "…" : ""), action: nil, keyEquivalent: "")
        entry.toolTip = String(body.prefix(1000))
        entry.isEnabled = false
        history.insert(entry, at: 0)
        menu.insertItem(entry, at: 2)
        if history.count > 5, let old = history.popLast() { menu.removeItem(old) }
    }
}
