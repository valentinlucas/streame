import AVFoundation
import SwiftUI

/// Aperçu caméra (calque AVFoundation, même session que la capture).
struct PreviewView: UIViewRepresentable {
    let camera: CameraCapture

    func makeUIView(context: Context) -> PreviewUIView {
        let view = PreviewUIView()
        view.videoPreviewLayer.session = camera.session
        view.videoPreviewLayer.videoGravity = .resizeAspect
        camera.attachPreview(view.videoPreviewLayer)
        return view
    }

    func updateUIView(_ uiView: PreviewUIView, context: Context) {}
}

final class PreviewUIView: UIView {
    override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }
    var videoPreviewLayer: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
}

/// Écran de réglages : un panneau compact et translucide sur l'aperçu caméra, qui tient en
/// entier sur un écran d'iPhone en paysage (pas de défilement, sauf pour dégager le clavier
/// quand un champ est en édition). Régie à gauche, téléphone à droite, direct en bas.
struct SetupView: View {
    @Environment(StreameModel.self) private var model
    @FocusState private var editing: Bool

    var body: some View {
        @Bindable var model = model
        GeometryReader { geo in
            ScrollView(.vertical) {
                VStack(spacing: 0) {
                    HStack {
                        HStack(spacing: 6) {
                            Text(model.camera.info)
                            if model.thermalLimited { Label("720p", systemImage: "thermometer.high").foregroundStyle(.orange) }
                            if model.camera.interrupted { Label("interrompue", systemImage: "pause.circle").foregroundStyle(.orange) }
                        }
                        .font(.caption.monospacedDigit())
                        .padding(6)
                        .background(.black.opacity(0.5), in: Capsule())
                        Spacer()
                    }
                    Spacer(minLength: 8)
                    panel($model)
                }
                .padding(12)
                .frame(minHeight: geo.size.height)
            }
            .scrollBounceBehavior(.basedOnSize)
            .scrollDismissesKeyboard(.interactively)
        }
        .toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("OK") { editing = false }
            }
        }
        // Panneau de commande : taille de texte bornée pour tenir sur un écran.
        .dynamicTypeSize(.small ... .large)
    }

    private func panel(_ model: Bindable<StreameModel>) -> some View {
        VStack(spacing: 10) {
            HStack(alignment: .top, spacing: 20) {
                // --- Régie -----------------------------------------------------------------
                VStack(alignment: .leading, spacing: 8) {
                    SetupTitle("Régie")
                    SetupRow("Mac") {
                        Picker("Mac", selection: model.selectedServerID) {
                            Text("Adresse manuelle").tag("")
                            ForEach(model.wrappedValue.browser.servers) { s in
                                Text("\(s.name) (\(s.host))").tag(s.id)
                            }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    if model.wrappedValue.selectedServer == nil {
                        SetupRow("Adresse") {
                            SetupField {
                                TextField("IP ou nom du Mac", text: model.manualHost)
                                    .textInputAutocapitalization(.never)
                                    .autocorrectionDisabled()
                                    .keyboardType(.URL)
                                    .submitLabel(.done)
                                    .focused($editing)
                            }
                        }
                        SetupRow("Port") {
                            SetupField {
                                TextField("8443", value: model.port, format: .number)
                                    .keyboardType(.numberPad)
                                    .focused($editing)
                            }
                            .frame(width: 90)
                            Spacer()
                        }
                    }
                    if let change = model.wrappedValue.certificateChange {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Le certificat de \(change.host) a changé.")
                                .font(.footnote).foregroundStyle(.orange)
                            Text(change.fingerprint)
                                .font(.caption2.monospaced()).foregroundStyle(.secondary)
                                .lineLimit(1).truncationMode(.middle)
                            Button("Accepter le nouveau certificat") { model.wrappedValue.acceptNewCertificate() }
                                .font(.footnote)
                        }
                    } else if let host = model.wrappedValue.target?.host, model.wrappedValue.storedFingerprint(for: host) != nil {
                        Button("Oublier le certificat mémorisé") { model.wrappedValue.forgetCertificate(for: host) }
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    if let problem = model.wrappedValue.browser.problem {
                        Text(problem).font(.footnote).foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                Divider().overlay(.white.opacity(0.2))

                // --- Téléphone -------------------------------------------------------------
                VStack(alignment: .leading, spacing: 8) {
                    SetupTitle(model.wrappedValue.name)
                    SetupRow("Objectif") {
                        Picker("Objectif", selection: model.lensID) {
                            ForEach(model.wrappedValue.camera.lenses) { Text($0.name).tag($0.id) }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    SetupRow("Qualité") {
                        Picker("Qualité", selection: model.quality) {
                            ForEach(Quality.all) { Text($0.label).tag($0.id) }
                        }
                        .labelsHidden()
                        .pickerStyle(.segmented)
                    }
                    Toggle(isOn: model.bluetoothMic) {
                        VStack(alignment: .leading, spacing: 1) {
                            Text("AirPods : micro et son (HFP)")
                            Text(model.wrappedValue.bluetoothMic
                                 ? "Oreillette imposée, bande étroite (16 kHz)."
                                 : "Son en A2DP, micro de l'iPhone.")
                                .font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .font(.subheadline)
                    .controlSize(.small)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            Divider().overlay(.white.opacity(0.2))

            // --- Direct --------------------------------------------------------------------
            HStack(spacing: 16) {
                Text(model.wrappedValue.status)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button {
                    model.wrappedValue.goLive()
                } label: {
                    Label("Passer en direct", systemImage: "dot.radiowaves.left.and.right")
                        .font(.headline)
                        .padding(.horizontal, 8)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .disabled(!model.wrappedValue.canGoLive)
            }
        }
        .padding(14)
        .background(.black.opacity(0.55), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(.white.opacity(0.12)))
    }
}

/// Titre de colonne du panneau de réglages.
private struct SetupTitle: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text.uppercased())
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .padding(.bottom, 2)
    }
}

/// Ligne « libellé + contrôle » du panneau.
private struct SetupRow<Content: View>: View {
    let label: String
    @ViewBuilder let content: Content
    init(_ label: String, @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }
    var body: some View {
        HStack(spacing: 8) {
            Text(label)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .frame(width: 64, alignment: .leading)
            content
        }
        .frame(minHeight: 30)
    }
}

/// Champ de saisie sur fond sombre.
private struct SetupField<Content: View>: View {
    @ViewBuilder let content: Content
    var body: some View {
        content
            .font(.subheadline)
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .background(.white.opacity(0.1), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}

/// Écran du direct : aperçu plein écran, cadre et badge « à l'antenne », trois boutons.
struct LiveView: View {
    @Environment(StreameModel.self) private var model
    @State private var confirmQuit = false

    private var connectionColor: Color {
        model.connected ? .green : .orange
    }

    var body: some View {
        ZStack {
            if model.onAir {
                Rectangle().strokeBorder(Color.red, lineWidth: 8).ignoresSafeArea()
            }
            VStack {
                HStack {
                    if model.onAir {
                        Text("À L'ANTENNE")
                            .font(.headline.bold())
                            .padding(.horizontal, 12).padding(.vertical, 6)
                            .background(Color.red, in: Capsule())
                    }
                    Spacer()
                    HStack(spacing: 8) {
                        Circle().fill(connectionColor).frame(width: 8, height: 8)
                        Text(model.liveStatusLine)
                        if model.thermalLimited { Image(systemName: "thermometer.high").foregroundStyle(.orange) }
                        if model.camera.interrupted { Image(systemName: "pause.circle").foregroundStyle(.orange) }
                    }
                    .font(.footnote.monospacedDigit())
                    .padding(.horizontal, 10).padding(.vertical, 6)
                    .background(.black.opacity(0.5), in: Capsule())
                }
                .padding()
                Spacer()
                HStack(spacing: 28) {
                    LiveButton(icon: model.micOn ? "mic.fill" : "mic.slash.fill",
                               label: model.micOn ? "Micro" : "Coupé", off: !model.micOn) { model.toggleMic() }
                    LiveButton(icon: model.speakerOn ? "speaker.wave.2.fill" : "speaker.slash.fill",
                               label: model.speakerOn ? "Son" : "Coupé", off: !model.speakerOn) { model.toggleSpeaker() }
                    LiveButton(icon: "xmark", label: "Quitter", off: false, tint: .red) { confirmQuit = true }
                }
                .padding(.bottom, 20)
            }
        }
        .persistentSystemOverlays(.hidden)
        // Retour haptique au passage à l'antenne (et à la sortie).
        .sensoryFeedback(trigger: model.onAir) { _, onAir in onAir ? .success : .warning }
        // Un tap involontaire ne coupe pas le direct, surtout à l'antenne.
        .confirmationDialog(model.onAir ? "Vous êtes à l'antenne. Arrêter le direct ?" : "Arrêter le direct ?",
                            isPresented: $confirmQuit, titleVisibility: .visible) {
            Button("Arrêter le direct", role: .destructive) { model.endLive() }
            Button("Continuer", role: .cancel) {}
        }
    }
}

struct LiveButton: View {
    let icon: String
    let label: String
    let off: Bool
    var tint: Color = .white
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 4) {
                Image(systemName: icon).font(.title2)
                Text(label).font(.caption)
            }
            .frame(width: 84, height: 64)
            .foregroundStyle(off ? Color.orange : tint)
            .background(.black.opacity(0.55), in: RoundedRectangle(cornerRadius: 14))
        }
        .accessibilityLabel(label)
    }
}
