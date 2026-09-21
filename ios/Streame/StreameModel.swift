import AVFoundation
import Foundation
import Observation
import SwiftUI
import UIKit

/// Statistiques d'émission renvoyées par la bibliothèque (même JSON que la page web).
struct PhoneStats: Decodable {
    var width: Int
    var height: Int
    var fps: Double
    var bitrate_kbps: Double
    var quality_limitation: String
    var rtt_ms: Double?
    var codec: String
}

struct Quality: Identifiable, Hashable {
    let id: Int
    let label: String
    static let all = [Quality(id: 1080, label: "1080p"), Quality(id: 720, label: "720p"), Quality(id: 480, label: "480p")]
}

/// Pointeur vers le client Rust, partagé avec la file de capture (immuable une fois créé).
private final class ClientHandle: @unchecked Sendable {
    let ptr: OpaquePointer
    init(_ ptr: OpaquePointer) { self.ptr = ptr }
}

/// Contexte opaque du callback C (le modèle, non retenu : il vit autant que l'app).
private struct CallbackContext: @unchecked Sendable {
    let raw: UnsafeMutableRawPointer
}

/// Callback C de la bibliothèque (thread interne) : renvoie l'événement sur le thread principal
/// **dans l'ordre d'émission** (file principale, FIFO — une `Task` par événement ne le
/// garantirait pas), avec le handle émetteur pour ignorer les événements d'un client arrêté.
private let eventCallback: streame_event_cb = { ctx, client, kind, json in
    guard let ctx, let client, let json else { return }
    let text = String(cString: json)
    let model = Unmanaged<StreameModel>.fromOpaque(ctx).takeUnretainedValue()
    DispatchQueue.main.async {
        MainActor.assumeIsolated { model.handleEvent(from: client, kind: kind, json: text) }
    }
}

/// État de l'app : réglages (persistés), aperçu caméra, découverte Bonjour, session Rust.
///
/// Threads : le modèle vit sur le thread principal ; tout ce qui bloque (démarrage et arrêt du
/// client Rust, moteur audio) passe par la file sérielle `fr.lvlab.streame.session`, jamais par
/// le pool coopératif de Swift Concurrency ni par le thread principal.
@Observable
@MainActor
final class StreameModel {
    // Réglages
    /// Nom affiché sur la régie : celui du téléphone, sans réglage (voir `DeviceName`).
    let name = DeviceName.current
    var manualHost: String { didSet { defaults.set(manualHost, forKey: "host") } }
    var port: Int { didSet { defaults.set(port, forKey: "port") } }
    var selectedServerID: String { didSet { defaults.set(selectedServerID, forKey: "server") } }
    var quality: Int { didSet { defaults.set(quality, forKey: "quality"); reconfigureCamera() } }
    var lensID: String { didSet { defaults.set(lensID, forKey: "lens"); reconfigureCamera() } }
    /// AirPods en HFP forcé (micro et sortie sur l'oreillette, bande étroite), par défaut ;
    /// sinon sortie A2DP et micro de l'iPhone.
    var bluetoothMic: Bool { didSet { defaults.set(bluetoothMic, forKey: "bluetoothMic") } }

    // État du direct
    private(set) var live = false
    private(set) var connected = false
    private(set) var onAir = false
    private(set) var micOn = true
    private(set) var speakerOn = true
    private(set) var status = "Prêt."
    private(set) var stats: PhoneStats?
    /// L'iPhone chauffe : capture plafonnée à 720p jusqu'au retour à la normale.
    private(set) var thermalLimited = false
    /// La régie `host` a présenté un certificat différent de celui mémorisé : à accepter
    /// explicitement (`acceptNewCertificate`) après vérification.
    private(set) var certificateChange: (host: String, fingerprint: String)?

    let camera = CameraCapture()
    let browser = BonjourBrowser()

    @ObservationIgnored private let defaults = UserDefaults.standard
    @ObservationIgnored private let sessionQueue = DispatchQueue(label: "fr.lvlab.streame.session", qos: .userInitiated)
    @ObservationIgnored private var client: ClientHandle?
    @ObservationIgnored private var audio: AudioEngineIO?
    @ObservationIgnored private var starting = false
    /// Clients arrêtés dont des événements tardifs peuvent encore arriver.
    @ObservationIgnored private var retired: Set<OpaquePointer> = []
    /// Hôte du direct en cours (mémorisation du certificat).
    @ObservationIgnored private var liveHost: String?
    @ObservationIgnored private var thermalObserver: NSObjectProtocol?

    init() {
        // Débogage : STREAME_RESET_SETTINGS=1 efface les réglages persistés (adresse, port…).
        if ProcessInfo.processInfo.environment["STREAME_RESET_SETTINGS"] == "1" {
            for key in ["host", "port", "server", "quality", "lens", "bluetoothMic", "certs"] { defaults.removeObject(forKey: key) }
            trace("réglages persistés effacés")
        }
        defaults.removeObject(forKey: "name") // ancien réglage, remplacé par le nom du téléphone
        manualHost = defaults.string(forKey: "host") ?? ""
        port = defaults.object(forKey: "port") as? Int ?? 8443
        selectedServerID = defaults.string(forKey: "server") ?? ""
        quality = defaults.object(forKey: "quality") as? Int ?? 1080
        lensID = defaults.string(forKey: "lens") ?? ""
        bluetoothMic = defaults.object(forKey: "bluetoothMic") as? Bool ?? true
        thermalLimited = Self.isHot(ProcessInfo.processInfo.thermalState)
        thermalObserver = NotificationCenter.default.addObserver(
            forName: ProcessInfo.thermalStateDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.thermalStateChanged() }
        }
    }

    // MARK: - Régie

    var selectedServer: DiscoveredServer? {
        browser.servers.first { $0.id == selectedServerID }
    }

    /// Hôte et port de la régie : serveur Bonjour choisi, sinon saisie manuelle.
    var target: (host: String, port: Int)? {
        if let s = selectedServer { return (s.host, s.port) }
        let h = manualHost.trimmingCharacters(in: .whitespaces)
        return h.isEmpty ? nil : (h, port)
    }

    var canGoLive: Bool { target != nil && !live }

    // MARK: - Aperçu

    /// Hauteur de capture effective : la qualité choisie, plafonnée à 720p quand l'iPhone chauffe.
    var effectiveHeight: Int { thermalLimited ? min(quality, 720) : quality }

    func startPreview() async {
        trace("startPreview : demande des autorisations")
        let cam = await AVCaptureDevice.requestAccess(for: .video)
        let mic = await AVCaptureDevice.requestAccess(for: .audio)
        trace("autorisations : caméra \(cam), micro \(mic)")
        guard cam else {
            status = "Caméra refusée : autoriser Streame dans Réglages."
            return
        }
        if !mic { status = "Micro refusé : la vidéo partira sans son." }
        camera.refreshLenses()
        if lensID.isEmpty || !camera.lenses.contains(where: { $0.id == lensID }) {
            lensID = camera.lenses.first?.id ?? ""
        }
        camera.configure(lensID: lensID, height: effectiveHeight)
        camera.start()
        browser.start()
        trace("aperçu démarré, objectif \(lensID), qualité \(quality)")
        // Débogage sans toucher l'écran : DEVICECTL_CHILD_STREAME_AUTOSTART=hôte:port
        // (ou variable STREAME_AUTOSTART) → passage en direct automatique après l'aperçu.
        if let auto = ProcessInfo.processInfo.environment["STREAME_AUTOSTART"], !auto.isEmpty {
            let parts = auto.split(separator: ":")
            let host = String(parts[0])
            let p = parts.count > 1 ? Int(parts[1]) ?? 8443 : 8443
            trace("autostart vers \(host):\(p) dans 1,5 s (réglages non modifiés)")
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            goLive(overriding: (host, p))
        }
    }

    private func reconfigureCamera() {
        guard !lensID.isEmpty else { return }
        camera.configure(lensID: lensID, height: effectiveHeight)
    }

    private static func isHot(_ state: ProcessInfo.ThermalState) -> Bool {
        state == .serious || state == .critical
    }

    private func thermalStateChanged() {
        let hot = Self.isHot(ProcessInfo.processInfo.thermalState)
        guard hot != thermalLimited else { return }
        thermalLimited = hot
        trace("thermique : \(hot ? "chauffe, capture plafonnée à 720p" : "retour à la normale")")
        reconfigureCamera()
    }

    /// Cycle de vie : en arrière-plan la caméra et l'audio sont suspendus par iOS, on termine
    /// proprement le direct (« bye » à la régie) plutôt que de laisser une session fantôme.
    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .background:
            if live { endLive(message: "Direct arrêté : app passée en arrière-plan.") }
        case .active:
            camera.start()
        default:
            break
        }
    }

    // MARK: - Certificat de la régie (mémorisé à la première connexion)

    private var storedFingerprints: [String: String] {
        get { defaults.dictionary(forKey: "certs") as? [String: String] ?? [:] }
        set { defaults.set(newValue, forKey: "certs") }
    }

    func storedFingerprint(for host: String) -> String? {
        storedFingerprints[host]
    }

    /// Accepte le nouveau certificat de la régie signalé par `certificateChange`.
    func acceptNewCertificate() {
        guard let change = certificateChange else { return }
        storedFingerprints[change.host] = change.fingerprint
        certificateChange = nil
        status = "Nouveau certificat de \(change.host) mémorisé."
    }

    func forgetCertificate(for host: String) {
        storedFingerprints[host] = nil
        certificateChange = nil
    }

    // MARK: - Direct

    func goLive(overriding override: (host: String, port: Int)? = nil) {
        let target = override ?? self.target
        trace("goLive : cible \(String(describing: target)), client \(client == nil ? "absent" : "présent"), starting \(starting)")
        guard let target, client == nil, !starting else { return }
        starting = true
        // L'écran du direct s'affiche tout de suite ; le démarrage (runtime, WebSocket, moteur
        // audio) se fait sur la file de session.
        live = true
        connected = false
        onAir = false
        micOn = true
        speakerOn = true
        stats = nil
        certificateChange = nil
        status = "Démarrage…"
        liveHost = target.host
        UIApplication.shared.isIdleTimerDisabled = true
        var cfg: [String: Any] = [
            "host": target.host, "port": target.port, "name": name,
            "height": quality, "fps": 30, "audio": true,
            // Micro mono (traitement vocal) : 64 kb/s suffisent largement en Opus.
            "mic_bitrate": 64_000,
        ]
        if let fp = storedFingerprint(for: target.host) { cfg["cert_fingerprint"] = fp }
        guard let data = try? JSONSerialization.data(withJSONObject: cfg),
              let json = String(data: data, encoding: .utf8) else {
            starting = false
            live = false
            return
        }
        let ctx = CallbackContext(raw: Unmanaged.passUnretained(self).toOpaque())
        let bluetoothMic = self.bluetoothMic
        trace("goLive : écran direct affiché, démarrage du client Rust sur la file de session")
        sessionQueue.async {
            trace("streame_client_start…")
            let handle = streame_client_start(json, eventCallback, ctx.raw).map(ClientHandle.init)
            trace("streame_client_start → \(handle == nil ? "NULL" : "ok")")
            // Le moteur audio (session, traitement vocal) prend 1 à 3 s : ici, pas sur le
            // thread principal ni sur le pool coopératif.
            var engine: AudioEngineIO?
            if let handle {
                let e = AudioEngineIO(client: handle.ptr, bluetoothMic: bluetoothMic)
                do {
                    try e.start()
                    engine = e
                } catch {
                    trace("audio : \(error)")
                }
            }
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self.clientStarted(handle, audio: engine) }
            }
        }
    }

    private func clientStarted(_ handle: ClientHandle?, audio engine: AudioEngineIO?) {
        starting = false
        guard live else {
            // L'utilisateur a quitté pendant le démarrage.
            stopClient(handle, engine: engine)
            return
        }
        guard let handle else {
            status = "Impossible de démarrer la session (voir la console Xcode)."
            live = false
            liveHost = nil
            UIApplication.shared.isIdleTimerDisabled = false
            return
        }
        client = handle
        // Chaque image de la caméra part vers l'encodeur Rust (file de capture, jamais bloquant).
        trace("clientStarted : branchement de la caméra")
        camera.setFrameHandler { pixelBuffer, ptsNs in
            streame_client_push_video(handle.ptr, Unmanaged.passUnretained(pixelBuffer).toOpaque(), ptsNs)
        }
        audio = engine
        if engine == nil { status = "Audio indisponible (voir la console)." }
    }

    /// Arrêt du moteur audio puis du client (« bye » au Mac, fermeture, ≤ 3 s) : après l'image
    /// éventuellement en cours sur la file de capture, puis sur la file de session — la file de
    /// capture n'est jamais bloquée, le thread principal non plus.
    private func stopClient(_ handle: ClientHandle?, engine: AudioEngineIO?) {
        guard let handle else {
            sessionQueue.async { engine?.stop() }
            return
        }
        retired.insert(handle.ptr)
        let sessionQueue = self.sessionQueue
        camera.afterInFlightFrames {
            sessionQueue.async {
                engine?.stop()
                streame_client_stop(handle.ptr)
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { _ = self.retired.remove(handle.ptr) }
                }
            }
        }
    }

    func endLive(message: String = "Direct arrêté.") {
        trace("endLive : \(message)")
        // Plus aucun callback (images, audio) vers Rust avant de libérer le client.
        camera.setFrameHandler(nil)
        let engine = audio
        audio = nil
        let handle = client
        client = nil
        stopClient(handle, engine: engine)
        live = false
        connected = false
        onAir = false
        stats = nil
        status = message
        liveHost = nil
        UIApplication.shared.isIdleTimerDisabled = false
    }

    func toggleMic() {
        micOn.toggle()
        if let c = client { streame_client_set_mic(c.ptr, micOn) }
    }

    func toggleSpeaker() {
        speakerOn.toggle()
        if let c = client { streame_client_set_speaker(c.ptr, speakerOn) }
    }

    /// Ligne d'état du direct : statistiques si disponibles, sinon le dernier message.
    var liveStatusLine: String {
        guard connected, let st = stats else { return status }
        var s = "\(st.width)x\(st.height) · \(Int(st.fps.rounded())) i/s · \(String(format: "%.1f", st.bitrate_kbps / 1000)) Mb/s"
        if let rtt = st.rtt_ms { s += " · \(Int(rtt.rounded())) ms" }
        if st.quality_limitation.hasPrefix("bandwidth") { s += " · débit limité" }
        return s
    }

    // MARK: - Événements de la bibliothèque (thread principal, dans l'ordre)

    func handleEvent(from sender: OpaquePointer, kind: Int32, json: String) {
        // Événement d'un client arrêté, ou d'un autre client que celui du direct : ignoré.
        if retired.contains(sender) { return }
        if let current = client?.ptr, current != sender { return }
        if client == nil && !starting { return }
        if kind != STREAME_EVENT_STATS { trace("événement \(kind) : \(json)") }
        let obj = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any] ?? [:]
        let text = obj["text"] as? String ?? ""
        switch Int(kind) {
        case STREAME_EVENT_STATUS:
            status = text
        case STREAME_EVENT_CONNECTED:
            connected = true
            status = "Connecté."
        case STREAME_EVENT_DISCONNECTED:
            connected = false
            stats = nil
            status = "Déconnecté, nouvelle tentative…"
        case STREAME_EVENT_ON_AIR:
            onAir = obj["on"] as? Bool ?? false
        case STREAME_EVENT_STATS:
            stats = try? JSONDecoder().decode(PhoneStats.self, from: Data(json.utf8))
        case STREAME_EVENT_ERROR:
            status = "Erreur : \(text)"
        case STREAME_EVENT_ENDED:
            endLive(message: "Session terminée par le Mac (\(text)).")
        case STREAME_EVENT_CERT:
            let fingerprint = obj["fingerprint"] as? String ?? ""
            let trusted = obj["trusted"] as? Bool ?? false
            guard let host = liveHost, !fingerprint.isEmpty else { break }
            if trusted {
                if storedFingerprint(for: host) == nil {
                    storedFingerprints[host] = fingerprint
                    trace("certificat de \(host) mémorisé : \(fingerprint)")
                }
            } else {
                certificateChange = (host, fingerprint)
                endLive(message: "Le certificat de \(host) a changé : connexion refusée. Vérifiez la régie, puis acceptez le nouveau certificat.")
            }
        default:
            break
        }
    }
}
