// WebSocket to the Rust voice server (ws://127.0.0.1:47800/ws). Connected only while the island
// is in use: connecting is what warms the models; disconnecting lets them unload when idle.
import Foundation

final class SamSocket: NSObject, URLSessionWebSocketDelegate, @unchecked Sendable {
    var onJSON: (([String: Any]) -> Void)?
    var onAudio: ((Data) -> Void)?
    var onState: ((Bool) -> Void)?
    private var task: URLSessionWebSocketTask?
    private lazy var session = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
    private let url: URL
    private(set) var isOpen = false
    private var inflight = 0
    private let lock = NSLock()

    init(url: URL) { self.url = url }

    func connect() {
        guard task == nil else { return }
        var req = URLRequest(url: url)
        LocalAuth.sign(&req)
        let t = session.webSocketTask(with: req)
        t.maximumMessageSize = 1 << 24
        task = t
        t.resume()
        receive()
    }

    func disconnect() {
        task?.cancel(with: .normalClosure, reason: nil)
        task = nil
        isOpen = false
        onState?(false)
    }

    func send(json: [String: Any]) {
        guard let d = try? JSONSerialization.data(withJSONObject: json), let s = String(data: d, encoding: .utf8) else { return }
        task?.send(.string(s)) { _ in }
    }

    /// Mic frames: at most 8 in flight; a stalled socket drops audio instead of queueing.
    func send(audio: Data) {
        lock.lock(); let busy = inflight >= 8; if !busy { inflight += 1 }; lock.unlock()
        guard !busy, let t = task else { return }
        t.send(.data(audio)) { [weak self] _ in self?.lock.lock(); self?.inflight -= 1; self?.lock.unlock() }
    }

    private func receive() {
        task?.receive { [weak self] r in
            guard let self else { return }
            switch r {
            case .success(.data(let d)): self.onAudio?(d)
            case .success(.string(let s)):
                if let d = s.data(using: .utf8), let j = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any] { self.onJSON?(j) }
            case .success: break
            case .failure:
                self.task = nil; self.isOpen = false; self.onState?(false); return
            }
            self.receive()
        }
    }

    func urlSession(_ s: URLSession, webSocketTask: URLSessionWebSocketTask, didOpenWithProtocol p: String?) {
        isOpen = true; onState?(true)
    }
    func urlSession(_ s: URLSession, webSocketTask: URLSessionWebSocketTask, didCloseWith c: URLSessionWebSocketTask.CloseCode, reason: Data?) {
        task = nil; isOpen = false; onState?(false)
    }
}
