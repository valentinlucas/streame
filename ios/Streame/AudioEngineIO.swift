import AVFoundation
import Foundation

/// Drapeau atomique (C11) partagé entre la file audio et les callbacks temps réel.
final class AtomicFlag: @unchecked Sendable {
    private let raw: OpaquePointer

    init(_ initial: Bool) {
        raw = streame_flag_create(initial)!
    }

    deinit { streame_flag_destroy(raw) }

    var value: Bool {
        get { streame_flag_load(raw) }
        set { streame_flag_store(raw, newValue) }
    }
}

/// Micro et sortie par `AVAudioEngine` avec le **traitement vocal Apple** (annulation d'écho,
/// gain automatique, réduction de bruit) — ce que fait libwebrtc sur iPhone. Le PCM part vers
/// la bibliothèque Rust et en revient depuis les callbacks temps réel (`AVAudioSinkNode`,
/// `AVAudioSourceNode`) ; Opus, anneaux et WebRTC restent en Rust.
///
/// Threads : tout ce qui touche la session et le graphe (démarrage, relance, arrêt) tourne sur
/// la file `fr.lvlab.streame.audio`, jamais sur le thread principal. Sur changement de
/// configuration (AirPods branchés, interruption terminée, réinitialisation des services
/// média) le graphe est **reconstruit** avec les formats relus : les callbacks capturent la
/// fréquence d'entrée de leur graphe, jamais une valeur périmée.
final class AudioEngineIO: @unchecked Sendable {
    private let engine = AVAudioEngine()
    private let client: OpaquePointer
    private let bluetoothMic: Bool
    private let queue = DispatchQueue(label: "fr.lvlab.streame.audio", qos: .userInitiated)
    private var sinkNode: AVAudioSinkNode?
    private var sourceNode: AVAudioSourceNode?
    private var observers: [NSObjectProtocol] = []
    /// Coupe les callbacks pendant une reconstruction et avant l'arrêt (le pointeur Rust est
    /// libéré juste après).
    private let active = AtomicFlag(false)
    /// (file audio) Plus aucune relance après `stop()`.
    private var stopped = false
    private var rebuildAttempts = 0
    /// Bloc maximal traité par callback (= `MAX_BLOCK_FRAMES` côté Rust) ; tampons entrelacés
    /// pré-alloués : aucune allocation dans les callbacks temps réel.
    private static let capacityFrames = 4096
    private let inScratch = UnsafeMutablePointer<Float>.allocate(capacity: capacityFrames * 2)
    private let outScratch = UnsafeMutablePointer<Float>.allocate(capacity: capacityFrames * 2)

    /// - Parameter bluetoothMic: **forcer** le profil HFP (AirPods en micro **et** sortie, bande
    ///   étroite) : seul HFP est déclaré à la session, et l'entrée Bluetooth est sélectionnée
    ///   comme entrée préférée dès qu'elle existe. Sinon sortie Bluetooth A2DP (haute
    ///   fidélité) et micro de l'iPhone.
    init(client: OpaquePointer, bluetoothMic: Bool) {
        self.client = client
        self.bluetoothMic = bluetoothMic
    }

    deinit {
        inScratch.deallocate()
        outScratch.deallocate()
    }

    /// Démarrage synchrone (1 à 3 s : session, traitement vocal). À appeler hors du thread
    /// principal ; au retour les callbacks tournent.
    func start() throws {
        try queue.sync {
            trace("audio : configuration de la session")
            try Self.configureSession(bluetoothMic: bluetoothMic)
            try buildGraph()
            trace("audio : démarrage du moteur")
            try engine.start()
            active.value = true
            observe()
            trace("audio : moteur démarré")
        }
    }

    /// Arrêt synchrone : plus aucun callback au retour. À appeler hors du thread principal.
    func stop() {
        queue.sync {
            stopped = true
            active.value = false
            observers.forEach { NotificationCenter.default.removeObserver($0) }
            observers.removeAll()
            teardownGraph()
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
    }

    // MARK: - Graphe (file audio)

    private func buildGraph() throws {
        let input = engine.inputNode
        // Traitement vocal : à activer avant toute connexion et avant le démarrage du moteur.
        if !input.isVoiceProcessingEnabled {
            try input.setVoiceProcessingEnabled(true)
        }
        let inFormat = input.outputFormat(forBus: 0)
        guard inFormat.sampleRate > 0, inFormat.channelCount > 0 else {
            throw NSError(domain: "Streame", code: 2, userInfo: [NSLocalizedDescriptionKey: "format d'entrée indisponible"])
        }
        let inRate = UInt32(inFormat.sampleRate.rounded())
        let client = self.client
        let active = self.active
        let inScratch = self.inScratch
        let capacity = Self.capacityFrames

        // Micro → Rust (entrelacé, canaux du format d'entrée, fréquence de ce graphe).
        let sink = AVAudioSinkNode { _, frameCount, abl -> OSStatus in
            guard active.value else { return noErr }
            let frames = min(Int(frameCount), capacity)
            let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: abl))
            let channels = min(buffers.count, 2)
            guard channels > 0 else { return noErr }
            for c in 0..<channels {
                guard let plane = buffers[c].mData?.assumingMemoryBound(to: Float.self) else { continue }
                for f in 0..<frames { inScratch[f * channels + c] = plane[f] }
            }
            streame_client_push_audio(client, inScratch, frames, UInt32(channels), inRate)
            return noErr
        }
        engine.attach(sink)
        engine.connect(input, to: sink, format: inFormat)
        sinkNode = sink

        // Rust → sortie (format standard non entrelacé, stéréo si possible, fréquence matérielle).
        let hw = engine.outputNode.outputFormat(forBus: 0)
        let outRate = hw.sampleRate > 0 ? hw.sampleRate : 48_000
        let outChannels = max(1, min(Int(hw.channelCount), 2))
        guard let renderFormat = AVAudioFormat(standardFormatWithSampleRate: outRate, channels: AVAudioChannelCount(outChannels)) else {
            throw NSError(domain: "Streame", code: 1, userInfo: [NSLocalizedDescriptionKey: "format de sortie"])
        }
        let outScratch = self.outScratch
        let outRateInt = UInt32(outRate.rounded())
        let source = AVAudioSourceNode(format: renderFormat) { _, _, frameCount, abl -> OSStatus in
            let frames = min(Int(frameCount), capacity)
            let buffers = UnsafeMutableAudioBufferListPointer(abl)
            let channels = min(buffers.count, outChannels)
            if active.value {
                _ = streame_client_pull_audio(client, outScratch, frames, UInt32(channels), outRateInt)
            } else {
                for i in 0..<(frames * channels) { outScratch[i] = 0 }
            }
            for c in 0..<channels {
                guard let plane = buffers[c].mData?.assumingMemoryBound(to: Float.self) else { continue }
                for f in 0..<frames { plane[f] = outScratch[f * channels + c] }
            }
            return noErr
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: renderFormat)
        sourceNode = source

        engine.prepare()
        print("[streame] audio : entrée \(inFormat.channelCount) canal(aux) @ \(inRate) Hz, sortie \(outChannels) canaux @ \(outRateInt) Hz, traitement vocal actif")
    }

    private func teardownGraph() {
        engine.stop()
        if let s = sinkNode { engine.detach(s) }
        if let s = sourceNode { engine.detach(s) }
        sinkNode = nil
        sourceNode = nil
    }

    /// Reconstruit le graphe avec les formats courants et relance le moteur.
    private func rebuild(reason: String) {
        guard !stopped else { return }
        trace("audio : reconstruction du graphe (\(reason)), moteur en marche : \(engine.isRunning)")
        active.value = false
        teardownGraph()
        do {
            try AVAudioSession.sharedInstance().setActive(true)
            // AirPods connectés en cours de direct : on les impose en HFP avant de relire les formats.
            if bluetoothMic { Self.preferBluetoothInput() }
            try buildGraph()
            try engine.start()
            active.value = true
            rebuildAttempts = 0
            trace("audio : moteur relancé")
        } catch {
            rebuildAttempts += 1
            trace("audio : relance impossible (\(rebuildAttempts)) : \(error)")
            if rebuildAttempts <= 5 {
                queue.asyncAfter(deadline: .now() + 1) { [weak self] in self?.rebuild(reason: "nouvel essai") }
            }
        }
    }

    /// Les notifications arrivent sur le thread émetteur (`queue: nil`) : on renvoie sur la
    /// file audio, jamais sur le thread principal.
    private func observe() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: nil) { [weak self] _ in
            self?.queue.async { self?.rebuild(reason: "configuration du moteur") }
        })
        observers.append(center.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: nil) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  let type = AVAudioSession.InterruptionType(rawValue: raw) else { return }
            switch type {
            case .began:
                trace("audio : interruption (appel, autre app)")
            case .ended:
                self?.queue.async { self?.rebuild(reason: "fin d'interruption") }
            @unknown default:
                break
            }
        })
        observers.append(center.addObserver(forName: AVAudioSession.mediaServicesWereResetNotification, object: nil, queue: nil) { [weak self] _ in
            self?.queue.async {
                guard let self, !self.stopped else { return }
                // Les services média ont redémarré : session et traitement vocal à refaire.
                try? Self.configureSession(bluetoothMic: self.bluetoothMic)
                self.rebuild(reason: "réinitialisation des services média")
            }
        })
        observers.append(center.addObserver(forName: AVAudioSession.routeChangeNotification, object: nil, queue: nil) { note in
            let reason = (note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt)
                .flatMap(AVAudioSession.RouteChangeReason.init(rawValue:))
            trace("audio : changement de route (\(String(describing: reason)))")
        })
    }

    /// Session : lecture + enregistrement, 48 kHz demandés, haut-parleur par défaut, mode vidéo
    /// (traitement vocal). Bluetooth : HFP forcé (micro et sortie sur l'oreillette) ou A2DP
    /// (sortie seule, haute fidélité) selon `bluetoothMic`. Ne pas déclarer A2DP en mode HFP :
    /// avec les deux, iOS peut préférer A2DP et laisser le micro sur l'iPhone.
    static func configureSession(bluetoothMic: Bool) throws {
        let s = AVAudioSession.sharedInstance()
        let options: AVAudioSession.CategoryOptions = bluetoothMic
            ? [.allowBluetoothHFP, .defaultToSpeaker]
            : [.allowBluetoothA2DP, .defaultToSpeaker]
        try s.setCategory(.playAndRecord, mode: .videoChat, options: options)
        try s.setPreferredSampleRate(48_000)
        try s.setPreferredIOBufferDuration(0.010)
        try s.setActive(true)
        if bluetoothMic { preferBluetoothInput() }
    }

    /// Impose l'entrée Bluetooth HFP si un tel appareil est connecté : la sortie suit sur le
    /// même appareil. Sans appareil Bluetooth, on laisse iOS choisir (micro de l'iPhone).
    static func preferBluetoothInput() {
        let s = AVAudioSession.sharedInstance()
        guard let bt = s.availableInputs?.first(where: { $0.portType == .bluetoothHFP }) else {
            if s.preferredInput != nil { try? s.setPreferredInput(nil) }
            return
        }
        guard s.preferredInput?.uid != bt.uid else { return }
        do {
            try s.setPreferredInput(bt)
            trace("audio : entrée Bluetooth HFP imposée (\(bt.portName))")
        } catch {
            trace("audio : entrée Bluetooth HFP refusée : \(error)")
        }
    }
}
