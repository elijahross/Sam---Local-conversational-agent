// Apple on-device speech recognition (macOS 26 SpeechAnalyzer + SpeechTranscriber) as an STT
// backend for speech_server.py (`--stt apple`). The model lives in the system's speech service
// (a shared OS asset), so Sam loads no STT weights of its own.
//
// Protocol (stdin/stdout): each request is a little-endian u32 byte length followed by 16 kHz
// mono int16 PCM; each answer is one JSON line {"text": "...", "ms": n} or {"error": "..."}.
// First line on start: {"ready": true, "locale": "en_US"} (after the locale's assets are
// installed; downloads them once if needed). `applestt --file x.wav` transcribes one file.
// `--locale en_US,de_DE`: every utterance runs through each language at once and the transcript
// with the higher word confidence wins (auto-detect between the user's languages).
import AVFoundation
import Foundation
import Speech

let args = CommandLine.arguments
func arg(_ k: String) -> String? { args.firstIndex(of: k).flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } }

func emit(_ o: [String: Any]) {
    let d = try! JSONSerialization.data(withJSONObject: o)
    FileHandle.standardOutput.write(d + Data([10]))
}

func readExactly(_ n: Int) -> Data? {
    var out = Data()
    while out.count < n {
        let d = FileHandle.standardInput.readData(ofLength: n - out.count)
        if d.isEmpty { return nil }
        out.append(d)
    }
    return out
}

let src = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 16000, channels: 1, interleaved: false)!

func buffer(pcm: Data) -> AVAudioPCMBuffer {
    let n = pcm.count / 2
    let b = AVAudioPCMBuffer(pcmFormat: src, frameCapacity: AVAudioFrameCount(max(n, 1)))!
    b.frameLength = AVAudioFrameCount(n)
    pcm.withUnsafeBytes { raw in
        let s = raw.bindMemory(to: Int16.self)
        for i in 0..<n { b.floatChannelData![0][i] = Float(Int16(littleEndian: s[i])) / 32768 }
    }
    return b
}

func convert(_ b: AVAudioPCMBuffer, to f: AVAudioFormat) throws -> AVAudioPCMBuffer {
    if b.format == f { return b }
    guard let c = AVAudioConverter(from: b.format, to: f) else { throw NSError(domain: "applestt", code: 1, userInfo: [NSLocalizedDescriptionKey: "no converter"]) }
    let cap = AVAudioFrameCount(Double(b.frameLength) * f.sampleRate / b.format.sampleRate) + 1024
    let out = AVAudioPCMBuffer(pcmFormat: f, frameCapacity: cap)!
    var fed = false
    var err: NSError?
    c.convert(to: out, error: &err) { _, status in
        if fed { status.pointee = .endOfStream; return nil }
        fed = true; status.pointee = .haveData; return b
    }
    if let err { throw err }
    return out
}

@available(macOS 26.0, *)
func setup(_ locale: Locale) async throws -> (Locale, AVAudioFormat) {
    guard let loc = await SpeechTranscriber.supportedLocale(equivalentTo: locale) else { throw NSError(domain: "applestt", code: 2, userInfo: [NSLocalizedDescriptionKey: "locale not supported: \(locale.identifier)"]) }
    let t = SpeechTranscriber(locale: loc, preset: .transcription)
    if await AssetInventory.status(forModules: [t]) != .installed, let req = try await AssetInventory.assetInstallationRequest(supporting: [t]) {
        try await req.downloadAndInstall()
    }
    guard let f = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [t]) else { throw NSError(domain: "applestt", code: 3, userInfo: [NSLocalizedDescriptionKey: "no audio format"]) }
    return (loc, f)
}

@available(macOS 26.0, *)
func transcribe(_ b: AVAudioPCMBuffer, locale: Locale, format: AVAudioFormat) async throws -> (text: String, confidence: Double) {
    let t = SpeechTranscriber(locale: locale, transcriptionOptions: [], reportingOptions: [], attributeOptions: [.transcriptionConfidence])
    let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
    let analyzer = SpeechAnalyzer(inputSequence: stream, modules: [t], options: .init(priority: .userInitiated, modelRetention: .lingering))
    let collect = Task { () -> (String, Double) in
        var s = "", sum = 0.0, n = 0.0
        for try await r in t.results {
            s += String(r.text.characters)
            for run in r.text.runs {
                if let c = run.transcriptionConfidence {
                    let w = Double(r.text[run.range].characters.count)
                    sum += c * w; n += w
                }
            }
        }
        return (s, n > 0 ? sum / n : 0)
    }
    cont.yield(AnalyzerInput(buffer: try convert(b, to: format)))
    cont.finish()
    try await analyzer.finalizeAndFinishThroughEndOfInput()
    let (text, conf) = try await collect.value
    return (text.trimmingCharacters(in: .whitespacesAndNewlines), conf)
}

/// Every configured language at once; the most confident transcript wins.
@available(macOS 26.0, *)
func best(_ b: AVAudioPCMBuffer, _ langs: [(Locale, AVAudioFormat)]) async throws -> (text: String, confidence: Double, locale: String) {
    try await withThrowingTaskGroup(of: (String, Double, String).self) { g in
        for (loc, fmt) in langs { g.addTask { let r = try await transcribe(b, locale: loc, format: fmt); return (r.text, r.confidence, loc.identifier) } }
        var top: (String, Double, String) = ("", -1, "")
        for try await r in g where r.1 > top.1 || (top.0.isEmpty && !r.0.isEmpty) { top = r }
        return top
    }
}

guard #available(macOS 26.0, *) else { emit(["error": "needs macOS 26"]); exit(1) }
// Default: the system language plus English (if different).
let defaultLocales = Locale.current.language.languageCode?.identifier == "en" ? Locale.current.identifier : "\(Locale.current.identifier),en_US"
let wants = (arg("--locale").flatMap { $0.isEmpty ? nil : $0 } ?? defaultLocales).split(separator: ",").map { Locale(identifier: $0.trimmingCharacters(in: .whitespaces)) }
let sem = DispatchSemaphore(value: 0)
Task {
    do {
        var langs: [(Locale, AVAudioFormat)] = []
        for w in wants { langs.append(try await setup(w)) }
        if let file = arg("--file") {
            let f = try AVAudioFile(forReading: URL(fileURLWithPath: file))
            let b = AVAudioPCMBuffer(pcmFormat: f.processingFormat, frameCapacity: AVAudioFrameCount(f.length))!
            try f.read(into: b)
            let t0 = Date()
            let r = try await best(b, langs)
            emit(["text": r.text, "ms": Int(Date().timeIntervalSince(t0) * 1000), "locale": r.locale, "confidence": r.confidence])
            exit(0)
        }
        emit(["ready": true, "locale": langs.map { $0.0.identifier }.joined(separator: ",")])
        while let h = readExactly(4) {
            let n = Int(h.withUnsafeBytes { $0.loadUnaligned(as: UInt32.self).littleEndian })
            guard let pcm = readExactly(n) else { break }
            let t0 = Date()
            do {
                let r = n < 3200 ? ("", 0.0, "") : try await best(buffer(pcm: pcm), langs)
                emit(["text": r.0, "ms": Int(Date().timeIntervalSince(t0) * 1000), "locale": r.2, "confidence": r.1])
            } catch { emit(["error": "\(error.localizedDescription)"]) }
        }
        exit(0)
    } catch {
        emit(["error": "\(error.localizedDescription)"]); exit(1)
    }
}
sem.wait()
