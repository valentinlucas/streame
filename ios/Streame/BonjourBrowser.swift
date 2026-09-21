import Foundation
import Network
import Observation

struct DiscoveredServer: Identifiable, Hashable {
    /// Nom d'instance Bonjour (unique sur le réseau).
    let id: String
    let name: String
    let host: String
    let port: Int
}

/// Découverte des régies (`_streame._tcp`, annoncées par streame sur le Mac). Chaque service
/// trouvé est résolu en adresse IP par une connexion TCP éphémère (l'API Network ne donne pas
/// l'adresse autrement), IPv4 de préférence. Les callbacks (légers) arrivent sur le thread
/// principal, d'où sont publiées les propriétés observées.
@Observable
final class BonjourBrowser {
    private(set) var servers: [DiscoveredServer] = []
    /// Message si la découverte est impossible (accès au réseau local refusé, par exemple).
    private(set) var problem: String?

    @ObservationIgnored private var browser: NWBrowser?
    @ObservationIgnored private var resolving: [String: NWConnection] = [:]

    func start() {
        guard browser == nil else { return }
        let params = NWParameters.tcp
        params.includePeerToPeer = true
        let b = NWBrowser(for: .bonjour(type: "_streame._tcp", domain: nil), using: params)
        b.stateUpdateHandler = { [weak self] state in
            trace("Bonjour : état \(state)")
            switch state {
            case .waiting(let e), .failed(let e):
                // kDNSServiceErr_PolicyDenied (-65570) : l'accès au réseau local est refusé.
                if case .dns(let code) = e, code == -65570 {
                    self?.problem = "Accès au réseau local refusé : Réglages > Streame > Réseau local."
                } else {
                    self?.problem = "Découverte Bonjour indisponible : \(e.localizedDescription)"
                }
            case .ready:
                self?.problem = nil
            default:
                break
            }
        }
        b.browseResultsChangedHandler = { [weak self] results, _ in
            self?.update(results)
        }
        b.start(queue: .main)
        browser = b
    }

    func stop() {
        browser?.cancel()
        browser = nil
        resolving.values.forEach { $0.cancel() }
        resolving.removeAll()
    }

    private func update(_ results: Set<NWBrowser.Result>) {
        var seen = Set<String>()
        trace("Bonjour : \(results.count) service(s) : \(results.map { "\($0.endpoint)" })")
        for r in results {
            guard case .service(let name, _, _, _) = r.endpoint else { continue }
            seen.insert(name)
            if resolving[name] == nil, !servers.contains(where: { $0.id == name }) {
                resolve(name: name, endpoint: r.endpoint)
            }
        }
        servers.removeAll { !seen.contains($0.id) }
    }

    private func resolve(name: String, endpoint: NWEndpoint) {
        let params = NWParameters.tcp
        if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let connection = NWConnection(to: endpoint, using: params)
        resolving[name] = connection
        trace("Bonjour : résolution de « \(name) »")
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self else { return }
            trace("Bonjour : « \(name) » → \(state), distant \(String(describing: connection?.currentPath?.remoteEndpoint))")
            switch state {
            case .ready:
                if let remote = connection?.currentPath?.remoteEndpoint,
                   case .hostPort(let host, let port) = remote,
                   let hostText = Self.text(for: host) {
                    let server = DiscoveredServer(id: name, name: name, host: hostText, port: Int(port.rawValue))
                    trace("Bonjour : « \(name) » = \(hostText):\(port.rawValue)")
                    if !self.servers.contains(server) {
                        self.servers.removeAll { $0.id == name }
                        self.servers.append(server)
                        self.servers.sort { $0.name < $1.name }
                    }
                }
                connection?.cancel()
                self.resolving[name] = nil
            case .failed, .cancelled:
                self.resolving[name] = nil
            default:
                break
            }
        }
        connection.start(queue: .main)
    }

    /// Adresse utilisable par la bibliothèque Rust : texte nu, sans suffixe d'interface
    /// (`%en0`, que `description` ajoute) ; pas d'adresse lien-local (AWDL, 169.254.x.x).
    private static func text(for host: NWEndpoint.Host) -> String? {
        switch host {
        case .ipv4(let a):
            if a.isLinkLocal { return nil }
            return a.rawValue.map { String($0) }.joined(separator: ".")
        case .ipv6(let a):
            if a.isLinkLocal { return nil }
            let bytes = [UInt8](a.rawValue)
            return stride(from: 0, to: 16, by: 2)
                .map { String(format: "%x", (UInt16(bytes[$0]) << 8) | UInt16(bytes[$0 + 1])) }
                .joined(separator: ":")
        case .name(let n, _): return n
        @unknown default: return nil
        }
    }
}
