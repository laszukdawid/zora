import AppKit
import SwiftUI

/// Names, coding agent, project folder and phrases. Reads and writes through `zorad settings`, which validates the change;
/// the running core picks it up within a second.
@MainActor
final class SettingsWindow: NSObject, NSWindowDelegate {
    private let model: SettingsModel
    private var window: NSWindow?
    private var activeObserver: NSObjectProtocol?

    init(cli: ZoradCLI, saved: @escaping () -> Void) {
        model = SettingsModel(cli: cli)
        super.init()
        model.close = { [weak self] in self?.window?.close() }
        model.saved = saved
    }

    func show() {
        if window == nil {
            let w = NSWindow(contentViewController: NSHostingController(rootView: SettingsView(model: model)))
            w.title = "Zora Settings"
            w.styleMask = [.titled, .closable]
            w.isReleasedWhenClosed = false
            w.delegate = self
            w.center()
            window = w
        }
        if !(window?.isVisible ?? false) { Task { await model.load() } }
        // A menu-bar-only app can't take keyboard focus, so it becomes a regular app while Settings is open.
        NSApp.setActivationPolicy(.regular)
        window?.makeKeyAndOrderFront(nil)
        window?.orderFrontRegardless()
        // The policy change lands asynchronously; the window can only become key once the app is active.
        if !NSApp.isActive && activeObserver == nil {
            activeObserver = NotificationCenter.default.addObserver(
                forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
            ) { [weak self] _ in MainActor.assumeIsolated { self?.becameActive() } }
        }
        NSApp.activate()
    }

    private func becameActive() {
        if let activeObserver { NotificationCenter.default.removeObserver(activeObserver) }
        activeObserver = nil
        window?.makeKeyAndOrderFront(nil)
    }

    /// A save in progress finishes first, so its result can't land on a window opened again later.
    func windowShouldClose(_ sender: NSWindow) -> Bool { !model.busy }

    func windowWillClose(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }
}

@MainActor @Observable
final class SettingsModel {
    var names: [String] = []
    var newName = ""
    var workspace = ""
    var harness = "claude" {
        didSet {
            guard harness != oldValue else { return }
            // Each agent keeps its own unsaved folder choice while the picker moves between them.
            folderDrafts[oldValue] = workspace
            workspace = folderDrafts[harness] ?? folder(for: harness)
        }
    }
    private var folderDrafts: [String: String] = [:]
    var customCommand = ""
    var start = ""
    var end = ""
    var cancel = ""
    var problems: [String] = []
    var error: String?
    var loaded = false
    var busy = false
    var close: () -> Void = {}
    var saved: () -> Void = {}
    private let cli: ZoradCLI
    /// What was loaded, so Save sends only what changed here and never undoes an edit made elsewhere meanwhile.
    private var original: CoreSettings?

    init(cli: ZoradCLI) { self.cli = cli }

    func load() async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        error = nil
        do {
            apply(try await cli.settings())
        } catch {
            loaded = false
            self.error = "Couldn't read the settings: \(error)"
            return
        }
        loaded = true
    }

    func addName() {
        let n = newName.trimmingCharacters(in: .whitespaces).lowercased()
        guard !n.isEmpty else { return }
        if !names.contains(n) { names.append(n) }
        newName = ""
    }

    func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "The folder your coding agent works in"
        if !workspace.isEmpty { panel.directoryURL = URL(fileURLWithPath: (workspace as NSString).expandingTildeInPath) }
        if panel.runModal() == .OK, let url = panel.url { workspace = url.path }
    }

    func save() async {
        guard !busy, let original else { return }
        addName() // a name typed but not yet added still counts
        // `expect` holds the loaded value of each changed field; zorad refuses the change if one differs on disk.
        var change: [String: Any] = [:]
        var expect: [String: Any] = [:]
        if names != original.wakeNames {
            change["wakeNames"] = names
            expect["wakeNames"] = original.wakeNames
        }
        let command = customCommand.trimmingCharacters(in: .whitespaces)
        if harness == "custom" && command != original.customCommand { change["customCommand"] = command }
        if harness != original.harness {
            change["harness"] = harness
            expect["harness"] = original.harness
        }
        // Only a folder changed here is sent; otherwise the agent keeps its own (or takes the current one).
        if workspace != folder(for: harness) {
            change["workspace"] = workspace
            expect["workspace"] = original.workspace
            expect["harness"] = original.harness
        }
        var phrases: [String: [String]] = [:]
        var oldPhrases: [String: [String]] = [:]
        for (key, text, old) in [("start", start, original.phrases.start), ("end", end, original.phrases.end),
                                 ("cancel", cancel, original.phrases.cancel)] where lines(text) != old {
            phrases[key] = lines(text)
            oldPhrases[key] = old
        }
        if !phrases.isEmpty {
            change["phrases"] = phrases
            expect["phrases"] = oldPhrases
        }
        guard !change.isEmpty else { return close() }
        guard let json = Self.json(change.merging(["expect": expect]) { a, _ in a }) else { return }
        busy = true
        error = nil
        let r = await cli.run(["settings", "set", json])
        busy = false
        guard r.ok else {
            if r.code == 3 { // zorad: the file changed since it was loaded
                // Someone else saved meanwhile: show their values rather than overwrite them.
                await load()
                if error == nil { error = "Settings changed elsewhere — reloaded" }
            } else {
                error = r.message
            }
            return
        }
        guard let s = CoreSettings.decode(r.out) else { return close() }
        apply(s)
        saved()
        // A running Zora applies the file only when nothing in it needs attention.
        guard s.problems.isEmpty else {
            error = "Saved, but Zora keeps its current settings until the problems above are fixed."
            return
        }
        close()
    }

    private func apply(_ s: CoreSettings) {
        original = s
        newName = ""
        names = s.wakeNames
        harness = s.harness
        workspace = s.workspace
        folderDrafts = [:] // setting `harness` above stored a draft for the old choice
        customCommand = s.customCommand
        start = s.phrases.start.joined(separator: "\n")
        end = s.phrases.end.joined(separator: "\n")
        cancel = s.phrases.cancel.joined(separator: "\n")
        problems = s.problems
    }

    private static func json(_ object: [String: Any]) -> String? {
        (try? JSONSerialization.data(withJSONObject: object)).flatMap { String(data: $0, encoding: .utf8) }
    }

    /// The folder `harness` would use if saved now: its own, or (new or unset) the current agent's.
    private func folder(for id: String) -> String {
        guard let original else { return workspace }
        if id == original.harness { return original.workspace }
        if let own = original.harnessWorkspaces[id], !own.trimmingCharacters(in: .whitespaces).isEmpty { return own }
        return original.workspace
    }

    /// The built-in agents, plus the configured one when it was set up by hand under another id.
    var harnessChoices: [(id: String, label: String)] {
        var c = [("claude", "Claude Code"), ("codex", "Codex"), ("pi", "Pi"), ("custom", "Custom command")]
        if let id = original?.harness, !c.contains(where: { $0.0 == id }) { c.append((id, id)) }
        return c.map { (id: $0.0, label: $0.1) }
    }

    private func lines(_ text: String) -> [String] {
        text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }
}

struct SettingsView: View {
    @Bindable var model: SettingsModel
    @State private var showPhrases = false
    @FocusState private var nameFieldFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Form {
                Section {
                    ForEach(model.names, id: \.self) { name in
                        HStack {
                            Text(name.capitalized)
                            Spacer()
                            Button {
                                model.names.removeAll { $0 == name }
                            } label: {
                                Image(systemName: "minus.circle")
                            }
                            .buttonStyle(.borderless)
                            .help("Remove \(name.capitalized)")
                            .accessibilityLabel("Remove \(name.capitalized)")
                        }
                    }
                    HStack {
                        TextField("New name", text: $model.newName, prompt: Text("Add a name"))
                            .labelsHidden()
                            .focused($nameFieldFocused)
                            .onSubmit { model.addName() }
                        Button("Add") { model.addName() }
                            .disabled(model.newName.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                } header: {
                    Text("Names")
                } footer: {
                    Text("Zora answers to any of these: “Hey \(example) … Do it \(example)”. One word each, at least 3 letters.")
                        .foregroundStyle(.secondary)
                }

                Section {
                    Picker("Agent", selection: $model.harness) {
                        ForEach(model.harnessChoices, id: \.id) { Text($0.label).tag($0.id) }
                    }
                    if model.harness == "custom" {
                        TextField("Command", text: $model.customCommand, prompt: Text("opencode run {prompt}"))
                            .font(.body.monospaced())
                            .accessibilityLabel("Custom command")
                    }
                } header: {
                    Text("Coding agent")
                } footer: {
                    Text(model.harness == "custom"
                         ? "Your command goes where {prompt} is, or at the end. It runs without a shell; quote words that contain spaces."
                         : "Runs your voice commands. It must be installed and signed in.")
                        .foregroundStyle(.secondary)
                }

                Section {
                    HStack {
                        Text(model.workspace.isEmpty ? "Not set" : model.workspace)
                            .foregroundStyle(model.workspace.isEmpty ? .secondary : .primary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer()
                        Button("Choose…") { model.chooseFolder() }
                    }
                } header: {
                    Text("Project folder")
                } footer: {
                    Text("Your coding agent runs here and can change files in it.")
                        .foregroundStyle(.secondary)
                }

                Section {
                    DisclosureGroup("Phrases", isExpanded: $showPhrases) {
                        Text("One phrase per line. “zora” stands for every name above.")
                            .foregroundStyle(.secondary)
                        phraseEditor("Start", text: $model.start)
                        phraseEditor("Run", text: $model.end)
                        phraseEditor("Cancel", text: $model.cancel)
                    }
                }

                if !model.problems.isEmpty {
                    Section("Needs attention") {
                        ForEach(model.problems, id: \.self) { Text($0).foregroundStyle(.orange) }
                    }
                }
            }
            .formStyle(.grouped)
            .disabled(!model.loaded || model.busy)

            if let error = model.error {
                Text(error)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .padding(.horizontal, 20)
                    .accessibilityLabel("Error: \(error)")
            }
            HStack {
                if model.busy { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel") { model.close() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(model.busy)
                Button("Save") { Task { await model.save() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.loaded || model.busy)
            }
            .padding(20)
        }
        .frame(width: 480, height: 640)
        .onChange(of: model.loaded) { _, loaded in if loaded { nameFieldFocused = true } }
    }

    private var example: String { (model.names.first ?? "zora").capitalized }

    private func phraseEditor(_ title: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.subheadline)
            TextEditor(text: text)
                .font(.body)
                .frame(height: 60)
                .accessibilityLabel("\(title) phrases")
        }
    }
}
