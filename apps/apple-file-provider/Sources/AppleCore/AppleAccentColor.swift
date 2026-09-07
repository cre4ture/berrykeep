import Foundation

public enum AppleAccentColor {
    public static let defaultHex = "#14B8A6"

    public static let swatches = [
        defaultHex,
        "#2563EB",
        "#7C3AED",
        "#DB2777",
        "#EA580C",
        "#D4A017",
    ]

    public static func normalizedHex(_ value: String?) -> String? {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !trimmed.isEmpty else {
            return nil
        }

        let withoutHash = trimmed.first == "#" ? String(trimmed.dropFirst()) : trimmed
        let characters = Array(withoutHash)
        let expanded: String
        switch characters.count {
        case 3:
            expanded = characters.map { "\($0)\($0)" }.joined()
        case 6:
            expanded = withoutHash
        default:
            return nil
        }

        guard expanded.allSatisfy({ "0123456789abcdefABCDEF".contains($0) }) else {
            return nil
        }
        return "#\(expanded.uppercased())"
    }
}
