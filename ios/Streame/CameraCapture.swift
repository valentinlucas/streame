import AVFoundation
import Observation
import UIKit

/// Capture caméra : choix de l'objectif, 16:9 paysage, NV12, 30 i/s. Les images partent vers
/// l'encodeur Rust (VideoToolbox) via `setFrameHandler`, sur la file de capture.
///
/// Threads : toute la configuration de la session (entrées, préréglage, cadence, rotation de
/// la connexion de sortie) se fait sur la file `fr.lvlab.streame.camera` ; le thread principal
/// ne touche qu'au calque d'aperçu et aux propriétés observées (`lenses`, `info`, `interrupted`).
@Observable
final class CameraCapture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    struct Lens: Identifiable, Hashable {
        let id: String
        let name: String
    }

    @ObservationIgnored let session = AVCaptureSession()
    private(set) var lenses: [Lens] = []
    private(set) var info = ""
    /// Capture interrompue par le système (autre app, appel, multitâche).
    private(set) var interrupted = false

    @ObservationIgnored private let queue = DispatchQueue(label: "fr.lvlab.streame.camera", qos: .userInteractive)
    @ObservationIgnored private let output = AVCaptureVideoDataOutput()
    @ObservationIgnored private var input: AVCaptureDeviceInput?
    @ObservationIgnored private var rotationCoordinator: AVCaptureDevice.RotationCoordinator?
    @ObservationIgnored private var rotationObservations: [NSKeyValueObservation] = []
    @ObservationIgnored private var notificationObservers: [NSObjectProtocol] = []
    @ObservationIgnored private weak var previewLayer: AVCaptureVideoPreviewLayer?
    /// Destinataire des images, lu à chaque image sous verrou : changer de destinataire ne
    /// bloque jamais l'appelant (la file de capture peut être occupée par une reconfiguration).
    @ObservationIgnored private let handlerLock = NSLock()
    @ObservationIgnored private var handler: ((CVPixelBuffer, Int64) -> Void)?
    @ObservationIgnored private var framesSeen = 0

    override init() {
        super.init()
        // L'audio est géré par AVAudioEngine (AudioEngineIO), pas par la session de capture.
        session.automaticallyConfiguresApplicationAudioSession = false
        output.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange]
        output.alwaysDiscardsLateVideoFrames = true
        output.setSampleBufferDelegate(self, queue: queue)
        observeSession()
    }

    deinit {
        notificationObservers.forEach { NotificationCenter.default.removeObserver($0) }
    }

    /// Objectifs disponibles (grand angle, ultra grand angle, téléobjectif, avant).
    func refreshLenses() {
        let discovery = AVCaptureDevice.DiscoverySession(
            deviceTypes: [.builtInWideAngleCamera, .builtInUltraWideCamera, .builtInTelephotoCamera],
            mediaType: .video, position: .unspecified)
        let back = discovery.devices.filter { $0.position == .back }
        let front = discovery.devices.filter { $0.position == .front }
        lenses = (back + front).map { Lens(id: $0.uniqueID, name: $0.localizedName) }
    }

    func configure(lensID: String, height: Int, fps: Int = 30) {
        queue.async { self.reconfigure(lensID: lensID, height: height, fps: fps) }
    }

    func start() {
        queue.async {
            if !self.session.isRunning {
                trace("caméra : startRunning")
                self.session.startRunning()
                trace("caméra : en marche")
            }
        }
    }

    func stop() {
        queue.async { if self.session.isRunning { self.session.stopRunning() } }
    }

    /// Destinataire des images. Immédiat ; un appel en cours peut encore se terminer, d'où
    /// `afterInFlightFrames` pour libérer des ressources que le destinataire utilise.
    func setFrameHandler(_ h: ((CVPixelBuffer, Int64) -> Void)?) {
        handlerLock.lock()
        handler = h
        handlerLock.unlock()
    }

    /// Exécute `work` sur la file de capture, donc après l'image éventuellement en cours.
    /// `work` doit rester court : rien de bloquant sur cette file.
    func afterInFlightFrames(_ work: @escaping () -> Void) {
        queue.async(execute: work)
    }

    /// Le calque d'aperçu, pour aligner sa rotation (et son miroir) sur la capture.
    func attachPreview(_ layer: AVCaptureVideoPreviewLayer) {
        previewLayer = layer
        DispatchQueue.main.async { self.applyPreviewRotation() }
    }

    // MARK: - Erreurs et interruptions de la session

    /// Erreur d'exécution (réinitialisation des services média…) : relance sur la file de
    /// capture ; interruption (autre app, appel) : état exposé à l'interface, reprise
    /// automatique par iOS à la fin.
    private func observeSession() {
        let center = NotificationCenter.default
        notificationObservers.append(center.addObserver(forName: AVCaptureSession.runtimeErrorNotification, object: session, queue: nil) { [weak self] note in
            let error = note.userInfo?[AVCaptureSessionErrorKey] as? NSError
            trace("caméra : erreur d'exécution \(String(describing: error))")
            self?.queue.async {
                guard let self else { return }
                if !self.session.isRunning {
                    trace("caméra : relance après erreur")
                    self.session.startRunning()
                }
            }
        })
        notificationObservers.append(center.addObserver(forName: AVCaptureSession.wasInterruptedNotification, object: session, queue: .main) { [weak self] note in
            let reason = (note.userInfo?[AVCaptureSessionInterruptionReasonKey] as? Int)
                .flatMap(AVCaptureSession.InterruptionReason.init(rawValue:))
            trace("caméra : interrompue (\(String(describing: reason)))")
            self?.interrupted = true
        })
        notificationObservers.append(center.addObserver(forName: AVCaptureSession.interruptionEndedNotification, object: session, queue: .main) { [weak self] _ in
            trace("caméra : fin d'interruption")
            self?.interrupted = false
        })
    }

    // MARK: - Configuration (file de capture)

    private func preset(for height: Int) -> AVCaptureSession.Preset {
        switch height {
        case ...480: return .vga640x480
        case ...720: return .hd1280x720
        default: return .hd1920x1080
        }
    }

    private func reconfigure(lensID: String, height: Int, fps: Int) {
        trace("caméra : reconfiguration (\(height)p)")
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        if let old = input {
            session.removeInput(old)
            input = nil
        }
        guard let device = AVCaptureDevice(uniqueID: lensID)
                ?? AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .back),
              let newInput = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(newInput) else {
            DispatchQueue.main.async { self.info = "Caméra indisponible" }
            return
        }
        session.addInput(newInput)
        input = newInput
        let wanted = preset(for: height)
        session.sessionPreset = session.canSetSessionPreset(wanted) ? wanted : .hd1280x720
        if !session.outputs.contains(output), session.canAddOutput(output) {
            session.addOutput(output)
        }
        do {
            try device.lockForConfiguration()
            let duration = CMTime(value: 1, timescale: CMTimeScale(fps))
            device.activeVideoMinFrameDuration = duration
            device.activeVideoMaxFrameDuration = duration
            device.unlockForConfiguration()
        } catch {
            print("[streame] cadence : \(error)")
        }
        // Stabilisation standard (faible latence) ; la cinématique ajoute trop de retard.
        if let conn = output.connection(with: .video), conn.isVideoStabilizationSupported {
            conn.preferredVideoStabilizationMode = .standard
        }
        let dims = CMVideoFormatDescriptionGetDimensions(device.activeFormat.formatDescription)
        let label = "\(device.localizedName) · \(dims.width)x\(dims.height)"
        trace("caméra : \(label), préréglage \(session.sessionPreset.rawValue)")
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.info = label
            // Paysage : on suit l'horizon (iOS 17), jamais le portrait. Le coordinateur observe
            // l'appareil depuis le thread principal ; l'application à la connexion de sortie
            // repart sur la file de capture.
            let coordinator = AVCaptureDevice.RotationCoordinator(device: device, previewLayer: self.previewLayer)
            self.rotationCoordinator = coordinator
            self.rotationObservations = [
                coordinator.observe(\.videoRotationAngleForHorizonLevelCapture, options: [.new]) { [weak self] _, _ in
                    self?.applyCaptureRotation()
                },
                coordinator.observe(\.videoRotationAngleForHorizonLevelPreview, options: [.new]) { [weak self] _, _ in
                    self?.applyPreviewRotation()
                },
            ]
            self.applyCaptureRotation()
            self.applyPreviewRotation()
        }
    }

    /// Angle paysage (0 ou 180°) de la capture, appliqué sur la file de capture ; en portrait
    /// on garde le dernier angle paysage (l'app est verrouillée en paysage de toute façon).
    private func applyCaptureRotation() {
        guard let coordinator = rotationCoordinator else { return }
        let angle = coordinator.videoRotationAngleForHorizonLevelCapture
        guard angle == 0 || angle == 180 else { return }
        queue.async { [output] in
            if let conn = output.connection(with: .video), conn.isVideoRotationAngleSupported(angle),
               conn.videoRotationAngle != angle {
                conn.videoRotationAngle = angle
            }
        }
    }

    /// Angle et miroir de l'aperçu (thread principal, calque). L'aperçu montre l'image telle
    /// qu'elle est envoyée : pas de miroir sur la caméra avant.
    private func applyPreviewRotation() {
        guard let conn = previewLayer?.connection else { return }
        if conn.isVideoMirroringSupported, conn.automaticallyAdjustsVideoMirroring || conn.isVideoMirrored {
            conn.automaticallyAdjustsVideoMirroring = false
            conn.isVideoMirrored = false
        }
        guard let coordinator = rotationCoordinator else { return }
        let angle = coordinator.videoRotationAngleForHorizonLevelPreview
        if angle == 0 || angle == 180, conn.isVideoRotationAngleSupported(angle), conn.videoRotationAngle != angle {
            conn.videoRotationAngle = angle
        }
    }

    // MARK: AVCaptureVideoDataOutputSampleBufferDelegate (file de capture)

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        framesSeen += 1
        handlerLock.lock()
        let handler = self.handler
        handlerLock.unlock()
        if framesSeen == 1 || framesSeen % 300 == 0 { trace("caméra : image \(framesSeen)\(handler == nil ? " (pas de direct)" : "")") }
        guard let handler, let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let ns = Int64((CMTimeGetSeconds(pts) * 1_000_000_000).rounded())
        handler(pixelBuffer, ns)
    }
}
