// The local access token Sam's localhost services require (server, computer-use helper, test
// API): a random secret in a 0600 file that web pages can't read. Same file and rules as
// server/src/localauth.rs and the helper.
import Foundation
import Security

enum LocalAuth {
    static let path = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/Sam/local-token")
    static let token: String = {
        if let t = try? String(contentsOf: path, encoding: .utf8), t.trimmingCharacters(in: .whitespacesAndNewlines).count >= 32 { return t.trimmingCharacters(in: .whitespacesAndNewlines) }
        try? FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
        var bytes = [UInt8](repeating: 0, count: 32); _ = SecRandomCopyBytes(kSecRandomDefault, 32, &bytes)
        let t = bytes.map { String(format: "%02x", $0) }.joined()
        let fd = open(path.path, O_WRONLY | O_CREAT | O_EXCL, 0o600)
        if fd >= 0 { _ = t.withCString { write(fd, $0, strlen($0)) }; close(fd); return t }
        return (try? String(contentsOf: path, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines) ?? t
    }()

    static func sign(_ r: inout URLRequest) { r.setValue(token, forHTTPHeaderField: "X-Sam-Token") }

    /// Test API rule: localhost Host, no browser Origin, the token.
    static func allowed(_ head: [Substring], port: Int) -> Bool {
        var h: [String: String] = [:]
        for l in head.dropFirst() { if let i = l.firstIndex(of: ":") { h[l[..<i].lowercased()] = l[l.index(after: i)...].trimmingCharacters(in: .whitespaces) } }
        return (h["host"] == "127.0.0.1:\(port)" || h["host"] == "localhost:\(port)") && h["origin"] == nil && h["x-sam-token"] == token
    }
}
