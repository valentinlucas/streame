import UIKit

/// Nom affiché sur la régie : le nom donné par l'utilisateur à son iPhone quand l'app y a accès
/// (avant iOS 16, ou avec l'autorisation Apple `user-assigned-device-name`), sinon le nom
/// commercial du modèle (« iPhone 16 Pro »), sinon l'identifiant matériel (« iPhone17,1 »).
enum DeviceName {
    static var current: String {
        let userName = UIDevice.current.name.trimmingCharacters(in: .whitespaces)
        let generic = ["iPhone", "iPad", "iPod touch"]
        if !userName.isEmpty, !generic.contains(userName) { return userName }
        let id = machineIdentifier
        return models[id] ?? (id.isEmpty ? UIDevice.current.model : id)
    }

    private static var machineIdentifier: String {
        var sys = utsname()
        uname(&sys)
        return withUnsafeBytes(of: &sys.machine) { buf in
            String(decoding: buf.prefix { $0 != 0 }, as: UTF8.self)
        }
    }

    /// Identifiants matériels → noms commerciaux (iPhone depuis 2020, iPad récents).
    private static let models: [String: String] = [
        "iPhone12,1": "iPhone 11", "iPhone12,3": "iPhone 11 Pro", "iPhone12,5": "iPhone 11 Pro Max",
        "iPhone12,8": "iPhone SE 2", "iPhone13,1": "iPhone 12 mini", "iPhone13,2": "iPhone 12",
        "iPhone13,3": "iPhone 12 Pro", "iPhone13,4": "iPhone 12 Pro Max", "iPhone14,4": "iPhone 13 mini",
        "iPhone14,5": "iPhone 13", "iPhone14,2": "iPhone 13 Pro", "iPhone14,3": "iPhone 13 Pro Max",
        "iPhone14,6": "iPhone SE 3", "iPhone14,7": "iPhone 14", "iPhone14,8": "iPhone 14 Plus",
        "iPhone15,2": "iPhone 14 Pro", "iPhone15,3": "iPhone 14 Pro Max", "iPhone15,4": "iPhone 15",
        "iPhone15,5": "iPhone 15 Plus", "iPhone16,1": "iPhone 15 Pro", "iPhone16,2": "iPhone 15 Pro Max",
        "iPhone17,1": "iPhone 16 Pro", "iPhone17,2": "iPhone 16 Pro Max", "iPhone17,3": "iPhone 16",
        "iPhone17,4": "iPhone 16 Plus", "iPhone17,5": "iPhone 16e", "iPhone18,1": "iPhone 17 Pro",
        "iPhone18,2": "iPhone 17 Pro Max", "iPhone18,3": "iPhone 17", "iPhone18,4": "iPhone Air",
        "iPad13,16": "iPad Air 5", "iPad13,17": "iPad Air 5", "iPad14,3": "iPad Pro 11 (4e gén.)",
        "iPad14,5": "iPad Pro 12,9 (6e gén.)", "iPad14,8": "iPad Air 11 (M2)", "iPad14,10": "iPad Air 13 (M2)",
        "iPad16,3": "iPad Pro 11 (M4)", "iPad16,5": "iPad Pro 13 (M4)",
    ]
}
