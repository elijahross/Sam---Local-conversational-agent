// Activation keys. ⌥Space (Carbon RegisterEventHotKey: no permission) is always available:
// hold = push-to-talk, tap = toggle hands-free. fn (listen-only CGEventTap) needs Input
// Monitoring: hold = push-to-talk, double-tap = hands-free — but only while the island is
// already open (hover the notch or ⌥Space), because fn is also the modifier for volume,
// brightness and other keys. A key pressed while fn is held cancels the hold (fn+F11 etc.).
// Esc is registered only while the island is open, so it never steals Esc from other apps.
import AppKit
import Carbon.HIToolbox

@MainActor
final class Hotkeys {
    var onPTT: ((Bool) -> Void)?        // true = down, false = up
    /// fn was part of a key combo after push-to-talk had started: drop that audio.
    var onPTTCancel: (() -> Void)?
    /// Whether fn may start anything now (the island is open). fn+Shift dictation is a chord and always works.
    var fnArmed: () -> Bool = { true }
    private var fnCombo = false
    var onToggle: (() -> Void)?
    var onEscape: (() -> Void)?
    /// Dictation key held (true) / released (false): ⌥⇧Space, or fn+Shift.
    var onDictate: ((Bool) -> Void)?
    private var fnDictating = false
    private var refs: [UInt32: EventHotKeyRef] = [:]
    private var spaceDown: Date?
    private var spaceHeldPTT = false
    private var fnDown: Date?
    private var lastFnTap: Date?
    private var fnPTT = false
    private var fnTimer: Timer?
    private var tap: CFMachPort?

    static var shared: Hotkeys?

    init() {
        Hotkeys.shared = self
        var spec = [EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed)),
                    EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyReleased))]
        InstallEventHandler(GetApplicationEventTarget(), { _, ev, _ -> OSStatus in
            var hk = EventHotKeyID()
            GetEventParameter(ev, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID), nil, MemoryLayout<EventHotKeyID>.size, nil, &hk)
            let pressed = GetEventKind(ev) == UInt32(kEventHotKeyPressed)
            MainActor.assumeIsolated { Hotkeys.shared?.carbon(id: hk.id, pressed: pressed) }
            return noErr
        }, 2, &spec, nil, nil)
        register(id: 1, key: UInt32(kVK_Space), mods: UInt32(optionKey))
        register(id: 3, key: UInt32(kVK_Space), mods: UInt32(optionKey | shiftKey))   // dictation (hold)
    }

    private func register(id: UInt32, key: UInt32, mods: UInt32) {
        var ref: EventHotKeyRef?
        let hid = EventHotKeyID(signature: OSType(0x524F574E), id: id)   // 'ROWN'
        if RegisterEventHotKey(key, mods, hid, GetApplicationEventTarget(), 0, &ref) == noErr, let r = ref { refs[id] = r }
    }
    private func unregister(id: UInt32) {
        if let r = refs.removeValue(forKey: id) { UnregisterEventHotKey(r) }
    }

    /// Esc only while the island is open.
    func escapeActive(_ on: Bool) {
        if on, refs[2] == nil { register(id: 2, key: UInt32(kVK_Escape), mods: 0) }
        if !on { unregister(id: 2) }
    }

    private func carbon(id: UInt32, pressed: Bool) {
        if id == 2 { if pressed { onEscape?() }; return }
        if id == 3 { onDictate?(pressed); return }
        guard id == 1 else { return }
        if pressed {
            spaceDown = Date()
            // Held > 250 ms = push-to-talk; shorter = tap (toggle hands-free).
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
                guard let self, let d = self.spaceDown, Date().timeIntervalSince(d) >= 0.24, !self.spaceHeldPTT else { return }
                self.spaceHeldPTT = true; self.onPTT?(true)
            }
        } else {
            let held = spaceDown.map { Date().timeIntervalSince($0) } ?? 0
            spaceDown = nil
            if spaceHeldPTT { spaceHeldPTT = false; onPTT?(false) } else if held < 0.25 { onToggle?() }
        }
    }

    // MARK: fn / Globe (Input Monitoring)
    static var fnAvailable: Bool { CGPreflightListenEventAccess() }
    static func requestFn() { CGRequestListenEventAccess() }

    func enableFn() {
        guard tap == nil, Hotkeys.fnAvailable else { return }
        // flagsChanged for fn itself; keyDown + system-defined (14: volume/brightness/media) to spot combos.
        let mask = CGEventMask(1 << CGEventType.flagsChanged.rawValue) | CGEventMask(1 << CGEventType.keyDown.rawValue) | CGEventMask(1 << 14)
        tap = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly, eventsOfInterest: mask, callback: { _, type, ev, _ in
            if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
                if let t = Hotkeys.shared?.tap { CGEvent.tapEnable(tap: t, enable: true) }
            } else if type != .flagsChanged {
                DispatchQueue.main.async { Hotkeys.shared?.otherKey() }
            } else if ev.getIntegerValueField(.keyboardEventKeycode) == 63 {   // kVK_Function
                let down = ev.flags.contains(.maskSecondaryFn)
                let shift = ev.flags.contains(.maskShift)
                DispatchQueue.main.async { Hotkeys.shared?.fn(down, shift: shift) }
            }
            return Unmanaged.passUnretained(ev)
        }, userInfo: nil)
        guard let t = tap else { return }
        CFRunLoopAddSource(CFRunLoopGetMain(), CFMachPortCreateRunLoopSource(nil, t, 0), .commonModes)
        CGEvent.tapEnable(tap: t, enable: true)
    }

    func fn(_ down: Bool, shift: Bool = false) {
        // fn+Shift held = dictation (insert text where the cursor is), not a conversation.
        if down && shift { fnDictating = true; onDictate?(true); return }
        if !down && fnDictating { fnDictating = false; onDictate?(false); return }
        if down {
            fnCombo = false
            guard fnArmed() else { fnDown = nil; lastFnTap = nil; return }
            fnDown = Date()
            // Start push-to-talk only after 150 ms so a tap isn't a hold.
            fnTimer?.invalidate()
            fnTimer = Timer.scheduledTimer(withTimeInterval: 0.15, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { guard let self, self.fnDown != nil else { return }; self.fnPTT = true; self.onPTT?(true) }
            }
        } else {
            fnTimer?.invalidate()
            let held = fnDown.map { Date().timeIntervalSince($0) } ?? 1
            fnDown = nil
            if fnPTT { fnPTT = false; onPTT?(false); return }
            if held < 0.2 && !fnCombo {
                if let last = lastFnTap, Date().timeIntervalSince(last) < 0.35 { lastFnTap = nil; onToggle?() } else { lastFnTap = Date() }
            }
        }
    }

    /// Another key while fn is held: it's a combo (fn+volume, fn+arrow…), not Sam.
    func otherKey() {
        guard fnDown != nil, !fnDictating else { return }
        fnCombo = true
        fnTimer?.invalidate()
        fnDown = nil
        lastFnTap = nil
        if fnPTT { fnPTT = false; onPTTCancel?() }
    }
}
