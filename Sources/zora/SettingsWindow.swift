import AppKit
import SwiftUI

/// Names, project folder and phrases. Reads and writes through `zorad settings`, which validates the change;
/// the running core picks it up within a second.
@MainActor
final class SettingsWindow: NSObject, NSWindowDelegate {
    private let model: SettingsModel
    private var window: NSWindow?
    private var activeObserver: NSObjectProtocol?

    init(cli: ZoradCLI) {
        model = SettingsModel(cli: cli)
        super.init()
        model.close = { [weak self] in self?.window?.close() }
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
    var start = ""
    var end = ""
    var cancel = ""
    var problems: [String] = []
    var error: String?
    var loaded = false
    var busy = false
    var close: () -> Void = {}
    private let cli: ZoradCLI
    /// What was loaded, so Save sends only what changed here and never undoes an edit made elsewhere meanwhile.
    private var original: Settings?

    init(cli: ZoradCLI) { self.cli = cli }

    func load() async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        error = nil
        let r = await cli.run(["settings"])
        guard r.ok, let s = Self.decode(r.out) else {
            loaded = false
            error = "Couldn't read the settings: \(r.ok ? "unexpected output from zorad" : r.message)"
            return
        }
        apply(s)
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
        var change: [String: Any] = [:]
        if names != original.wakeNames { change["wakeNames"] = names }
        if workspace != original.workspace { change["workspace"] = workspace }
        var phrases: [String: [String]] = [:]
        for (key, text, old) in [("start", start, original.phrases.start), ("end", end, original.phrases.end),
                                 ("cancel", cancel, original.phrases.cancel)] where lines(text) != old {
            phrases[key] = lines(text)
        }
        if !phrases.isEmpty { change["phrases"] = phrases }
        guard !change.isEmpty else { return close() }
        guard let data = try? JSONSerialization.data(withJSONObject: change),
              let json = String(data: data, encoding: .utf8) else { return }
        busy = true
        defer { busy = false }
        error = nil
        let r = await cli.run(["settings", "set", json])
        guard r.ok else {
            error = r.message
            return
        }
        if let s = Self.decode(r.out) { apply(s) }
        close()
    }

    private struct Settings: Decodable {
        struct Phrases: Decodable { var start: [String]; var end: [String]; var cancel: [String] }
        var wakeNames: [String]
        var phrases: Phrases
        var workspace: String
        var problems: [String]
    }

    private static func decode(_ out: String) -> Settings? {
        try? JSONDecoder().decode(Settings.self, from: Data(out.utf8))
    }

    private func apply(_ s: Settings) {
        original = s
        newName = ""
        names = s.wakeNames
        workspace = s.workspace
        start = s.phrases.start.joined(separator: "\n")
        end = s.phrases.end.joined(separator: "\n")
        cancel = s.phrases.cancel.joined(separator: "\n")
        problems = s.problems
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
