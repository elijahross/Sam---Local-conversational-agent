// Mic → 16 kHz mono Int16 frames (20 ms) and streamed TTS (Int16 @ 24 kHz) → speakers.
// Voice processing (echo cancellation) so Sam's own voice doesn't trigger the VAD. The engine
// runs only while the island listens/speaks: stopping it turns the mic indicator off (R2).
import AVFoundation
import Accelerate
import CoreAudio

final class AudioIO: @unchecked Sendable {
    /// Another app records from the default input (a call, a voice memo): Sam stays quiet.
    /// Only meaningful while Sam's own engine is stopped (it would count itself).
    static func inputBusyElsewhere() -> Bool {
        var dev = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultInputDevice, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &dev) == noErr, dev != 0 else { return false }
        var running = UInt32(0)
        size = UInt32(MemoryLayout<UInt32>.size)
        addr.mSelector = kAudioDevicePropertyDeviceIsRunningSomewhere
        guard AudioObjectGetPropertyData(dev, &addr, 0, nil, &size, &running) == noErr else { return false }
        return running != 0
    }

    var onFrame: ((Data) -> Void)?          // 640 bytes = 20 ms @ 16 kHz Int16
    var onLevel: ((Float, [Float]) -> Void)?
    var onPlaybackIdle: (() -> Void)?

    private var engine = AVAudioEngine()
    private var player = AVAudioPlayerNode()
    /// Audio devices changed (headphones in/out…): the engine's formats are stale → rebuilt on
    /// the next start.
    private var stale = false
    private var configObserver: NSObjectProtocol?
    /// Echo cancellation off after it failed to start on this device setup (the server's echo
    /// guard still drops Sam's own voice).
    var voiceProcessing = true
    private var vpUsed = false
    private let outRate: Double
    private let ttsFormat: AVAudioFormat
    private var converter: AVAudioConverter?
    private var pending = Data()
    private(set) var micRunning = false
    private(set) var engineRunning = false
    private var scheduled = 0
    private let q = DispatchQueue(label: "sam.audio")
    private var rampSamples = 0
    private var injecting = false

    init(ttsRate: Double = 24_000) {
        outRate = ttsRate
        ttsFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: ttsRate, channels: 1, interleaved: false)!
        engine.attach(player)
        observeConfig()
    }

    private func observeConfig() {
        if let o = configObserver { NotificationCenter.default.removeObserver(o) }
        configObserver = NotificationCenter.default.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: nil) { [weak self] _ in
            self?.stale = true
        }
    }

    /// A fresh engine (after a device change or a failed start).
    private func rebuild() {
        if engineRunning || micRunning { if micRunning { engine.inputNode.removeTap(onBus: 0) }; player.stop(); engine.stop() }
        engineRunning = false; micRunning = false
        engine = AVAudioEngine(); player = AVAudioPlayerNode()
        engine.attach(player)
        observeConfig()
        stale = false
        voiceProcessing = true
    }

    static func micPermission() -> AVAuthorizationStatus { AVCaptureDevice.authorizationStatus(for: .audio) }
    static func requestMic(_ done: @escaping (Bool) -> Void) { AVCaptureDevice.requestAccess(for: .audio, completionHandler: done) }

    /// Starts playback (+ the mic unless `micOff`, e.g. while a test injects audio).
    /// Core Audio -10875 (failed initialization) happens when voice processing is switched on
    /// before the mixer exists, or on device setups where it can't run (e.g. separate input and
    /// output devices): rebuild once, then fall back to the mic without echo cancellation.
    func start(micOff: Bool = false) throws {
        if stale { rebuild() }
        if engineRunning && (micOff || micRunning) { return }
        do {
            try startOnce(micOff: micOff)
        } catch where !micOff {
            rebuild()
            do { try startOnce(micOff: false) } catch {
                rebuild(); voiceProcessing = false
                try startOnce(micOff: false)
            }
        }
    }

    private func startOnce(micOff: Bool) throws {
        if engineRunning { engine.stop(); engineRunning = false }
        _ = engine.mainMixerNode   // the mixer must exist before voice processing is enabled
        if !micOff && voiceProcessing && !engine.inputNode.isVoiceProcessingEnabled {
            try engine.inputNode.setVoiceProcessingEnabled(true)   // also applies to the output
            vpUsed = true
            // Voice processing ducks all other audio while it runs; advanced ducking at the
            // minimum level lowers it only while someone is actually speaking.
            if #available(macOS 14.0, *) {
                engine.inputNode.voiceProcessingOtherAudioDuckingConfiguration = .init(enableAdvancedDucking: true, duckingLevel: .min)
            }
        }
        engine.connect(player, to: engine.mainMixerNode, format: ttsFormat)
        if !micOff { try installTap() }
        engine.prepare()
        try engine.start()
        player.play()
        engineRunning = true
        micRunning = !micOff
    }

    private func installTap() throws {
        let input = engine.inputNode
        // Voice processing changes the input to multi-channel; ask the tap for MONO at the
        // native rate, then convert — feeding the raw format to the converter gives silence.
        let native = input.outputFormat(forBus: 0).sampleRate
        guard native > 0, input.outputFormat(forBus: 0).channelCount > 0 else {
            throw NSError(domain: "Sam", code: 1, userInfo: [NSLocalizedDescriptionKey: "no microphone input (check Sound settings › Input)"])
        }
        let tapFmt = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: native, channels: 1, interleaved: false)!
        let outFmt = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16_000, channels: 1, interleaved: true)!
        converter = AVAudioConverter(from: tapFmt, to: outFmt)
        input.removeTap(onBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: tapFmt) { [weak self] buf, _ in
            self?.convertAndEmit(buf, outFmt)
        }
    }

    private func convertAndEmit(_ buf: AVAudioPCMBuffer, _ outFmt: AVAudioFormat) {
        guard let conv = converter else { return }
        let cap = AVAudioFrameCount(Double(buf.frameLength) * 16_000 / buf.format.sampleRate) + 32
        guard let out = AVAudioPCMBuffer(pcmFormat: outFmt, frameCapacity: cap) else { return }
        var fed = false
        conv.convert(to: out, error: nil) { _, status in
            if fed { status.pointee = .noDataNow; return nil }
            fed = true; status.pointee = .haveData; return buf
        }
        guard out.frameLength > 0, let p = out.int16ChannelData else { return }
        emit(Data(bytes: p[0], count: Int(out.frameLength) * 2))
    }

    /// Splits into 20 ms frames, computes level + 24 bands for the waveform.
    private func emit(_ d: Data) {
        q.async { [self] in
            pending.append(d)
            while pending.count >= 640 {
                let frame = pending.prefix(640); pending.removeFirst(640)
                let f = Data(frame)
                onFrame?(f)
                let samples = f.withUnsafeBytes { Array($0.bindMemory(to: Int16.self)) }
                let floats = samples.map { Float($0) / 32768 }
                var rms: Float = 0; vDSP_rmsqv(floats, 1, &rms, vDSP_Length(floats.count))
                let lvl = min(1, rms * 6)
                let n = floats.count / 24
                let bands = (0..<24).map { i -> Float in
                    var r: Float = 0; floats[(i * n)..<((i + 1) * n)].withUnsafeBufferPointer { vDSP_rmsqv($0.baseAddress!, 1, &r, vDSP_Length(n)) }
                    return min(1, r * 8)
                }
                onLevel?(lvl, bands)
            }
        }
    }

    /// Test mode: stream a WAV as if it were the mic (real-time pace).
    func inject(wav url: URL, then done: @escaping () -> Void) {
        injecting = true
        DispatchQueue.global().async { [self] in
            guard let file = try? AVAudioFile(forReading: url),
                  let inBuf = AVAudioPCMBuffer(pcmFormat: file.processingFormat, frameCapacity: AVAudioFrameCount(file.length)),
                  (try? file.read(into: inBuf)) != nil else { injecting = false; done(); return }
            let outFmt = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16_000, channels: 1, interleaved: true)!
            let conv = AVAudioConverter(from: file.processingFormat, to: outFmt)!
            let out = AVAudioPCMBuffer(pcmFormat: outFmt, frameCapacity: AVAudioFrameCount(Double(file.length) * 16_000 / file.processingFormat.sampleRate) + 64)!
            var fed = false
            conv.convert(to: out, error: nil) { _, st in if fed { st.pointee = .endOfStream; return nil }; fed = true; st.pointee = .haveData; return inBuf }
            // A real mic keeps sending room silence after speech; the server's VAD needs that
            // pause to end the turn (smart-turn check at 250 ms, fallback TURN_TIMEOUT_MS = 1.5 s in tests),
            // so the injection continues with 2 s of silence.
            let all = Data(bytes: out.int16ChannelData![0], count: Int(out.frameLength) * 2) + Data(count: 16_000 * 2 * 2)
            var off = 0
            let start = Date()
            while off < all.count && injecting {
                let end = min(off + 640, all.count)
                var chunk = all.subdata(in: off..<end)
                if chunk.count < 640 { chunk.append(Data(count: 640 - chunk.count)) }
                emit(chunk)
                off = end
                let due = start.addingTimeInterval(Double(off / 640) * 0.02)
                let wait = due.timeIntervalSinceNow
                if wait > 0 { Thread.sleep(forTimeInterval: wait) }
            }
            injecting = false
            done()
        }
    }

    // MARK: playback
    /// A reply starts playing only once ~400 ms are queued, and after the queue ran dry it
    /// waits for ~250 ms again: a late TTS chunk then shifts the whole reply a little instead
    /// of leaving a gap in it (speech-footprint lab: a 300 ms pre-buffer removed every stall
    /// under load; the jitter the user hears is the player running dry chunk by chunk).
    private var buffered: [AVAudioPCMBuffer] = []
    private var bufferedFrames: AVAudioFrameCount = 0
    private var primed = false
    func beginReply() { rampSamples = 0; primed = false; buffered.removeAll(); bufferedFrames = 0 }
    private func prebufferFrames() -> AVAudioFrameCount { AVAudioFrameCount(outRate * (primed ? 0.25 : 0.4)) }

    func play(_ pcm16: Data) {
        // Speech after the mic was turned off: a playback-only engine (no voice processing, so
        // nothing else gets ducked).
        if !engineRunning { try? start(micOff: true) }
        let n = pcm16.count / 2
        guard n > 0, let buf = AVAudioPCMBuffer(pcmFormat: ttsFormat, frameCapacity: AVAudioFrameCount(n)) else { return }
        buf.frameLength = AVAudioFrameCount(n)
        let dst = buf.floatChannelData![0]
        pcm16.withUnsafeBytes { raw in
            let s = raw.bindMemory(to: Int16.self)
            for i in 0..<n {
                // Ramp the first 200 ms of a reply: echo cancellation needs time to converge.
                let ramp = min(1, Float(rampSamples + i) / Float(outRate * 0.2))
                dst[i] = Float(s[i]) / 32768 * ramp
            }
        }
        rampSamples += n
        var rms: Float = 0; vDSP_rmsqv(dst, 1, &rms, vDSP_Length(n))
        onLevel?(min(1, rms * 5), [])
        // Hold chunks until the pre-buffer is full while nothing is queued; once playing, pass them straight through.
        let queued = q.sync { scheduled }
        if queued == 0 {
            buffered.append(buf); bufferedFrames += buf.frameLength
            if bufferedFrames < prebufferFrames() { return }
            primed = true
            let batch = buffered; buffered.removeAll(); bufferedFrames = 0
            for b in batch { schedule(b) }
        } else {
            schedule(buf)
        }
        if !player.isPlaying { player.play() }
    }

    private func schedule(_ buf: AVAudioPCMBuffer) {
        q.sync { scheduled += 1 }
        player.scheduleBuffer(buf) { [weak self] in
            guard let self else { return }
            var idle = false
            self.q.sync { self.scheduled -= 1; idle = self.scheduled == 0 }
            if idle {
                // Anything held back while the queue drained plays now (a short pause, no gap mid-word).
                DispatchQueue.main.async { [weak self] in
                    guard let self else { return }
                    if !self.buffered.isEmpty { let batch = self.buffered; self.buffered.removeAll(); self.bufferedFrames = 0; for b in batch { self.schedule(b) }; if !self.player.isPlaying { self.player.play() } }
                    else { self.onPlaybackIdle?() }
                }
            }
        }
    }

    var isPlaying: Bool { q.sync { scheduled > 0 } }

    /// Barge-in / close: drop queued speech immediately.
    /// Silent playback (voice off): the stream still "plays", so turn timing stays the same.
    func setMuted(_ on: Bool) {
        engine.mainMixerNode.outputVolume = on ? 0 : 1
    }

    func stopPlayback() {
        buffered.removeAll(); bufferedFrames = 0; primed = false
        player.stop(); q.sync { scheduled = 0 }
        if engineRunning { player.play() }
    }

    func stop() {
        injecting = false
        guard engineRunning else { return }
        // Touching inputNode instantiates the audio input (→ mic prompt / indicator): only if used.
        if micRunning { engine.inputNode.removeTap(onBus: 0) }
        player.stop()
        engine.stop()
        engineRunning = false; micRunning = false
        // Voice processing keeps its hidden aggregate device (VPAUAggregateAudioDevice) — and
        // with it the ducking of every other app's audio — as long as this engine exists, even
        // stopped. Switch it off and drop the engine now, not at the next start.
        if vpUsed {
            try? engine.inputNode.setVoiceProcessingEnabled(false)
            vpUsed = false
            rebuild()
        }
        q.sync { scheduled = 0; pending.removeAll() }
    }
}
