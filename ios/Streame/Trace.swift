import Foundation

private let traceStart = Date()
private let traceFormatter: DateFormatter = {
    let f = DateFormatter()
    f.dateFormat = "HH:mm:ss.SSS"
    return f
}()

/// Trace horodatée (console Xcode / devicectl --console), avec le thread et le temps écoulé
/// depuis le lancement — pour situer précisément chaque étape du passage en direct.
func trace(_ message: @autoclosure () -> String) {
    let now = Date()
    let elapsed = String(format: "%7.3f", now.timeIntervalSince(traceStart))
    let thread = Thread.isMainThread ? "main" : (Thread.current.name?.isEmpty == false ? Thread.current.name! : "bg")
    print("[streame \(traceFormatter.string(from: now)) +\(elapsed)s \(thread)] \(message())")
}
