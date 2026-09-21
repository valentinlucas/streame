import SwiftUI
import UIKit

/// L'app est verrouillée en paysage : l'image envoyée ne dépend pas de l'orientation.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication,
                     supportedInterfaceOrientationsFor window: UIWindow?) -> UIInterfaceOrientationMask {
        .landscape
    }
}

@main
struct StreameApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = StreameModel()

    init() {
        streame_log_init("info,streame_rtc=debug")
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .preferredColorScheme(.dark)
                .statusBarHidden(true)
        }
    }
}

struct RootView: View {
    @Environment(StreameModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            // Un seul calque d'aperçu, créé une fois : le passage en direct ne touche pas à la
            // session de capture (ré-attacher un calque la reconfigure et bloque le thread principal).
            PreviewView(camera: model.camera).ignoresSafeArea()
            if model.live {
                LiveView()
            } else {
                SetupView()
            }
        }
        .task { await model.startPreview() }
        .onChange(of: scenePhase) { _, phase in model.scenePhaseChanged(phase) }
    }
}
