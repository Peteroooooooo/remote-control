import SwiftUI
import UIKit

/// Lets SwiftUI controls reach the UIKit screen view (zoom reset, wheel, keyboard focus).
final class ScreenController: ObservableObject {
    @Published var zoomed = false
    weak var view: RemoteScreenView?

    func resetZoom() { view?.resetZoom() }
    /// Before wheel steps: in direct mode the host cursor goes to the last tapped spot so
    /// Windows scrolls the pane under the finger, not wherever the mouse was.
    func prepareWheel() { view?.prepareWheel() }
    /// Gives the hardware keyboard back to the remote screen.
    func focusScreen() { _ = view?.becomeFirstResponder() }
}

/// The remote desktop: video, host cursor, and all touch / pencil / trackpad / keyboard input.
/// Gestures follow the Android app (FullscreenScreen.kt):
///
/// Trackpad mode: 1-finger move = pointer with acceleration, tap = left click, hold ~0.4 s and
/// lift = right click, hold then drag (or tap then press and drag) = left drag, 2-finger tap =
/// right click, 2-finger pinch / drag = local zoom and pan, 3-finger tap = keyboard.
///
/// Direct mode: tap = touch tap at that spot, 1-finger drag = pan the zoomed picture, hold and
/// lift = right click, hold then drag = left drag (auto-pans at the edges), 2-finger pinch / drag
/// = zoom and pan, 3-finger tap = keyboard.
final class RemoteScreenView: UIView {
    // Inputs from SwiftUI.
    var streamSize = CGSize(width: 1920, height: 1080) { didSet { if streamSize != oldValue { relayout() } } }
    var monitorSize: CGSize?
    var touchMode: TouchMode = .trackpad { didSet { if touchMode != oldValue { updateCursor() } } }
    var cursorGlyph: CursorGlyph? { didSet { updateCursor() } }
    var cursorVisible = true { didSet { if cursorVisible != oldValue { updateCursor() } } }
    var cursorMetaKind = 0
    var onToggleKeyboard: () -> Void = {}
    weak var controller: ScreenController?

    private let bridge = RcBridge.shared
    private let videoLayer: CALayer
    private let cursorLayer = CALayer()
    private let dragRing = CAShapeLayer()

    // Local view of the picture.
    private var zoom: CGFloat = 1
    private var pan = CGPoint.zero
    private var cursorNorm = CGPoint(x: 0.5, y: 0.5)
    private var lastTapNorm = CGPoint(x: 0.5, y: 0.5)

    // Tuning, in points and milliseconds (Android uses the same numbers in dp).
    private let touchSlop: CGFloat = 8
    private let pinchSlop: CGFloat = 18
    private let doubleTapMaxDist: CGFloat = 44
    private let longPressMs: Double = 380
    private let doubleTapWindowMs: Double = 260
    private let autoPanMargin: CGFloat = 28

    private enum Pad { case pending, move, longPressed, dragging, longPressDragging, twoPending, twoPinch, threePending, done }
    private enum Direct { case pending, hold, holdDrag, pan, twoPinch, three, done }

    private var active: [UITouch] = []
    private var gestureActive = false
    private var gestureDirect = false
    private var pad = Pad.pending
    private var direct = Direct.pending
    private var startPos = CGPoint.zero
    private var downMs: Double = 0
    private var doubleTapCandidate = false
    private var lastTapUpMs: Double = 0
    private var lastTapUpPos = CGPoint.zero
    private var prevSinglePos = CGPoint.zero
    private var prevMoveMs: Double = 0
    private var lastMouseSentMs: Double = 0
    private var velocity = CGPoint.zero
    private var multiDownMs: Double = 0
    private var multiStartCentroid = CGPoint.zero
    private var multiStartSpan: CGFloat = 0
    private var prevCentroid: CGPoint?
    private var prevSpan: CGFloat = 0
    private var leftDown = false
    private var lastDragPos = CGPoint.zero
    private var longPressTimer: Timer?
    private var flingTimer: Timer?
    private var edgeTimer: Timer?
    private var flingVelocity = CGPoint.zero
    private var flingStepsLeft = 0

    // Apple Pencil and iPad trackpad / mouse.
    private var penTouch: UITouch?
    private var pointerTouch: UITouch?
    private var pointerButton = 0
    private var scrollRemainder = CGPoint.zero

    private let lightHaptic = UIImpactFeedbackGenerator(style: .light)
    private let mediumHaptic = UIImpactFeedbackGenerator(style: .medium)

    init(videoLayer: CALayer) {
        self.videoLayer = videoLayer
        super.init(frame: .zero)
        backgroundColor = .black
        isMultipleTouchEnabled = true
        layer.addSublayer(videoLayer)

        cursorLayer.magnificationFilter = .linear
        cursorLayer.isHidden = true
        cursorLayer.shadowColor = UIColor.black.cgColor
        cursorLayer.shadowOpacity = 0.25
        cursorLayer.shadowRadius = 1
        cursorLayer.shadowOffset = .zero
        layer.addSublayer(cursorLayer)

        dragRing.fillColor = UIColor.clear.cgColor
        dragRing.strokeColor = UIColor(red: 0.04, green: 0.52, blue: 1, alpha: 0.82).cgColor
        dragRing.lineWidth = 2
        dragRing.path = UIBezierPath(ovalIn: CGRect(x: -14, y: -14, width: 28, height: 28)).cgPath
        dragRing.isHidden = true
        layer.addSublayer(dragRing)

        let hover = UIHoverGestureRecognizer(target: self, action: #selector(onHover(_:)))
        addGestureRecognizer(hover)
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(onScroll(_:)))
        scroll.allowedScrollTypesMask = .all
        scroll.maximumNumberOfTouches = 0 // trackpad / mouse wheel only, never fingers
        addGestureRecognizer(scroll)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    // MARK: - Layout

    override func layoutSubviews() {
        super.layoutSubviews()
        relayout()
    }

    private var container: CGSize { bounds.size }

    private func relayout() {
        let clamped = ViewMapping.clampPan(pan, scale: zoom, container: container, stream: streamSize)
        pan = clamped
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if let f = ViewMapping.videoFrame(container: container, stream: streamSize, scale: zoom, pan: pan) {
            videoLayer.frame = f
        }
        CATransaction.commit()
        updateCursor()
        let z = zoom > 1.02
        if controller?.zoomed != z { controller?.zoomed = z }
    }

    func resetZoom() {
        zoom = 1
        pan = .zero
        relayout()
    }

    func prepareWheel() {
        if touchMode == .direct { bridge.mouseMove(lastTapNorm.x, lastTapNorm.y) }
    }

    private func toStream(_ p: CGPoint) -> CGPoint? {
        ViewMapping.screenToStream(p, container: container, stream: streamSize, scale: zoom, pan: pan)
    }

    private func toScreen(_ n: CGPoint) -> CGPoint? {
        ViewMapping.streamToScreen(n, container: container, stream: streamSize, scale: zoom, pan: pan)
    }

    private func updateCursor() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        let pos = toScreen(cursorNorm)
        if leftDown && !gestureDirect, let pos {
            dragRing.isHidden = false
            dragRing.position = pos
        } else {
            dragRing.isHidden = true
        }
        guard touchMode == .trackpad, cursorVisible, let g = cursorGlyph, let pos else {
            cursorLayer.isHidden = true
            return
        }
        var s = ViewMapping.hostToScreenScale(container: container, stream: streamSize, monitor: monitorSize, zoom: zoom)
        let longest = CGFloat(max(g.width, g.height)) * s
        if longest < 16 { s *= 16 / max(longest, 0.01) }
        cursorLayer.contents = g.image
        cursorLayer.frame = CGRect(
            x: pos.x - CGFloat(g.hotX) * s,
            y: pos.y - CGFloat(g.hotY) * s,
            width: CGFloat(g.width) * s,
            height: CGFloat(g.height) * s
        )
        cursorLayer.isHidden = false
    }

    private func ripple(at p: CGPoint, right: Bool) {
        let ring = CAShapeLayer()
        ring.path = UIBezierPath(ovalIn: CGRect(x: -22, y: -22, width: 44, height: 44)).cgPath
        ring.position = p
        ring.fillColor = (right ? UIColor.systemOrange : UIColor.white).withAlphaComponent(0.18).cgColor
        ring.strokeColor = (right ? UIColor.systemOrange : UIColor.white).withAlphaComponent(0.9).cgColor
        ring.lineWidth = 2
        layer.addSublayer(ring)
        let scale = CABasicAnimation(keyPath: "transform.scale")
        scale.fromValue = 0.45
        scale.toValue = 1
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 0.9
        fade.toValue = 0
        let group = CAAnimationGroup()
        group.animations = [scale, fade]
        group.duration = 0.35
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        ring.opacity = 0
        CATransaction.begin()
        CATransaction.setCompletionBlock { ring.removeFromSuperlayer() }
        ring.add(group, forKey: "ripple")
        CATransaction.commit()
    }

    // MARK: - Shared helpers

    private static func nowMs() -> Double { CACurrentMediaTime() * 1000 }

    private static func dist(_ a: CGPoint, _ b: CGPoint) -> CGFloat { hypot(a.x - b.x, a.y - b.y) }

    private func points() -> [CGPoint] { active.map { $0.location(in: self) } }

    private func mouseMoveThrottled(_ n: CGPoint, force: Bool = false) {
        let now = Self.nowMs()
        if force || now - lastMouseSentMs >= 7 {
            bridge.mouseMove(n.x, n.y)
            lastMouseSentMs = now
        }
    }

    private func click(_ button: Int) {
        bridge.mouseButton(button, down: true)
        bridge.mouseButton(button, down: false)
    }

    private func releaseLeft() {
        if leftDown {
            bridge.mouseButton(0, down: false)
            leftDown = false
            updateCursor()
        }
    }

    /// Moves the trackpad cursor by a finger delta with the acceleration curve.
    private func applyTrackpadDelta(_ dx: CGFloat, _ dy: CGFloat, dtMs: CGFloat) {
        let sw = max(streamSize.width, 1)
        let sh = max(streamSize.height, 1)
        let fitted = ViewMapping.fittedSize(container: container, stream: streamSize) ?? container
        let effW = max(fitted.width * zoom, 1)
        let effH = max(fitted.height * zoom, 1)
        let speed = hypot(dx, dy) / max(dtMs, 1)
        let gain = TrackpadCurve.gain(speed: speed, overInteractive: cursorMetaKind != 0)
        // Same thumb travel moves the same number of host pixels on both axes.
        let hostPerPoint = min(sw / effW, sh / effH)
        let step = 1.15 * gain * hostPerPoint
        cursorNorm = CGPoint(
            x: ViewMapping.clamp01(cursorNorm.x + dx * step / sw),
            y: ViewMapping.clamp01(cursorNorm.y + dy * step / sh)
        )
        autoPanForCursor()
        mouseMoveThrottled(cursorNorm)
        updateCursor()
    }

    /// Keeps the trackpad cursor inside the view while zoomed by sliding the picture.
    private func autoPanForCursor() {
        guard zoom > 1.02, let pos = toScreen(cursorNorm) else { return }
        var next = pan
        if pos.x < autoPanMargin { next.x += autoPanMargin - pos.x }
        else if pos.x > container.width - autoPanMargin { next.x -= pos.x - (container.width - autoPanMargin) }
        if pos.y < autoPanMargin { next.y += autoPanMargin - pos.y }
        else if pos.y > container.height - autoPanMargin { next.y -= pos.y - (container.height - autoPanMargin) }
        if next != pan {
            pan = ViewMapping.clampPan(next, scale: zoom, container: container, stream: streamSize)
            relayout()
        }
    }

    private func applyPinchPan(centroid: CGPoint, prev: CGPoint?, span: CGFloat, prevSpan: CGFloat, recenterCursor: Bool) {
        let ratio = prevSpan > 1 ? min(max(span / prevSpan, 0.75), 1.35) : 1
        let newScale = min(max(zoom * ratio, 1), 6)
        let dx = prev.map { centroid.x - $0.x } ?? 0
        let dy = prev.map { centroid.y - $0.y } ?? 0
        let factor = zoom > 0 ? newScale / zoom : 1
        let cx = centroid.x - container.width * 0.5
        let cy = centroid.y - container.height * 0.5
        let raw = CGPoint(x: (pan.x - cx) * factor + cx + dx, y: (pan.y - cy) * factor + cy + dy)
        if newScale <= 1.02 {
            zoom = 1
            pan = .zero
            relayout()
            return
        }
        zoom = newScale
        pan = ViewMapping.clampPan(raw, scale: newScale, container: container, stream: streamSize)
        relayout()
        guard recenterCursor else { return }
        // A pinch that pushed the cursor off screen brings it back to the centre, so the next
        // trackpad touch does not snap the view back to the old corner.
        let cur = toScreen(cursorNorm)
        if cur == nil || !bounds.contains(cur!) {
            if let c = toStream(CGPoint(x: container.width * 0.5, y: container.height * 0.5)) {
                cursorNorm = c
                bridge.mouseMove(c.x, c.y)
                updateCursor()
            }
        }
    }

    /// Direct-mode drag near an edge slides the zoomed picture that way. True when it moved.
    private func edgeAutoPan(_ pos: CGPoint) -> Bool {
        guard zoom > 1.02, container.width > 0, container.height > 0 else { return false }
        let margin: CGFloat = 48
        let maxStep: CGFloat = 14
        func step(_ v: CGFloat, _ size: CGFloat) -> CGFloat {
            if v < margin { return maxStep * (1 - max(v, 0) / margin) }
            if v > size - margin { return -maxStep * (1 - max(size - v, 0) / margin) }
            return 0
        }
        let d = CGPoint(x: step(pos.x, container.width), y: step(pos.y, container.height))
        guard d != .zero else { return false }
        let next = ViewMapping.clampPan(CGPoint(x: pan.x + d.x, y: pan.y + d.y), scale: zoom, container: container, stream: streamSize)
        guard next != pan else { return false }
        pan = next
        relayout()
        return true
    }

    // MARK: - UIKit touches

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        var fingersAdded = false
        for t in touches {
            switch t.type {
            case .pencil: penBegan(t)
            case .indirectPointer: pointerBegan(t, event: event)
            default:
                active.append(t)
                fingersAdded = true
            }
        }
        guard fingersAdded else { return }
        if !gestureActive {
            beginGesture()
        } else {
            step()
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        var fingerMoved = false
        for t in touches {
            if t === penTouch { penMoved(t) }
            else if t === pointerTouch { pointerMoved(t) }
            else if active.contains(where: { $0 === t }) { fingerMoved = true }
        }
        if fingerMoved && gestureActive { step() }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        lifted(touches, cancelled: false)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        lifted(touches, cancelled: true)
    }

    private func lifted(_ touches: Set<UITouch>, cancelled: Bool) {
        var fingerLifted = false
        for t in touches {
            if t === penTouch { penEnded(t) }
            else if t === pointerTouch { pointerEnded() }
            else if let i = active.firstIndex(where: { $0 === t }) {
                active.remove(at: i)
                fingerLifted = true
            }
        }
        guard fingerLifted, gestureActive else { return }
        if active.isEmpty {
            finishGesture(cancelled: cancelled)
        } else {
            step()
        }
    }

    private func beginGesture() {
        guard let first = active.first else { return }
        gestureActive = true
        flingTimer?.invalidate()
        flingTimer = nil
        startPos = first.location(in: self)
        downMs = Self.nowMs()
        gestureDirect = touchMode == .direct
        if gestureDirect {
            direct = .pending
            prevSinglePos = startPos
            lastDragPos = startPos
            prevCentroid = nil
        } else {
            pad = .pending
            doubleTapCandidate = downMs - lastTapUpMs <= doubleTapWindowMs && Self.dist(startPos, lastTapUpPos) <= doubleTapMaxDist
            prevSinglePos = startPos
            prevMoveMs = downMs
            lastMouseSentMs = 0
            velocity = .zero
        }
        lightHaptic.prepare()
        mediumHaptic.prepare()
        longPressTimer?.invalidate()
        longPressTimer = Timer.scheduledTimer(
            timeInterval: longPressMs / 1000, target: self, selector: #selector(longPressFired), userInfo: nil, repeats: false
        )
    }

    // Timers use target / selector so their callbacks run as plain main-thread methods.
    @objc private func longPressFired() {
        guard gestureActive else { return }
        if gestureDirect {
            guard direct == .pending else { return }
            direct = .hold
            mediumHaptic.impactOccurred()
            ripple(at: startPos, right: true)
        } else {
            guard pad == .pending else { return }
            pad = .longPressed
            mediumHaptic.impactOccurred()
            ripple(at: toScreen(cursorNorm) ?? startPos, right: true)
        }
    }

    private func step() {
        if gestureDirect { stepDirect() } else { stepPad() }
    }

    private func finishGesture(cancelled: Bool) {
        longPressTimer?.invalidate()
        longPressTimer = nil
        gestureActive = false
        if cancelled {
            // The system took the touches (e.g. a notification): never leave a button held.
            releaseLeft()
            if gestureDirect && direct == .holdDrag {
                bridge.mouseButton(0, down: false)
                direct = .done
            }
            edgeTimer?.invalidate()
            edgeTimer = nil
            return
        }
        if gestureDirect { finishDirect() } else { finishPad() }
    }

    // MARK: - Trackpad mode

    private func stepPad() {
        let pts = points()
        let now = Self.nowMs()
        if pts.count >= 3 {
            releaseLeft()
            let c = CGPoint(x: pts.map(\.x).reduce(0, +) / CGFloat(pts.count), y: pts.map(\.y).reduce(0, +) / CGFloat(pts.count))
            if pad != .threePending && pad != .done {
                pad = .threePending
                multiDownMs = now
                multiStartCentroid = c
            } else if pad == .threePending && Self.dist(c, multiStartCentroid) > touchSlop * 1.2 {
                pad = .done
            }
            return
        }
        if pts.count == 2 {
            releaseLeft()
            let c = CGPoint(x: (pts[0].x + pts[1].x) * 0.5, y: (pts[0].y + pts[1].y) * 0.5)
            let span = Self.dist(pts[0], pts[1])
            switch pad {
            case .pending, .move, .longPressed, .dragging, .longPressDragging:
                pad = .twoPending
                multiDownMs = now
                multiStartCentroid = c
                multiStartSpan = span
                prevCentroid = c
                prevSpan = span
            case .twoPending:
                if abs(span - multiStartSpan) > pinchSlop || Self.dist(c, multiStartCentroid) > touchSlop {
                    pad = .twoPinch
                    applyPinchPan(centroid: c, prev: prevCentroid, span: span, prevSpan: prevSpan, recenterCursor: true)
                    prevCentroid = c
                    prevSpan = span
                }
            case .twoPinch:
                applyPinchPan(centroid: c, prev: prevCentroid, span: span, prevSpan: prevSpan, recenterCursor: true)
                prevCentroid = c
                prevSpan = span
            case .threePending, .done:
                break
            }
            return
        }
        guard let p = pts.first else { return }
        switch pad {
        case .twoPending, .threePending, .done:
            break // let the last finger lift so the tap count stays right
        case .twoPinch:
            pad = .done
        case .pending:
            if Self.dist(p, startPos) > touchSlop {
                if doubleTapCandidate {
                    pad = .dragging
                    bridge.mouseMove(cursorNorm.x, cursorNorm.y)
                    bridge.mouseButton(0, down: true)
                    leftDown = true
                } else {
                    pad = .move
                }
                prevSinglePos = p
                prevMoveMs = now
                applyTrackpadDelta(p.x - startPos.x, p.y - startPos.y, dtMs: CGFloat(max(now - downMs, 1)))
            }
        case .move:
            let dx = p.x - prevSinglePos.x, dy = p.y - prevSinglePos.y
            let dt = CGFloat(max(now - prevMoveMs, 1))
            velocity = CGPoint(x: velocity.x * 0.45 + dx / dt * 0.55, y: velocity.y * 0.45 + dy / dt * 0.55)
            prevSinglePos = p
            prevMoveMs = now
            applyTrackpadDelta(dx, dy, dtMs: dt)
        case .longPressed:
            if Self.dist(p, startPos) > touchSlop {
                pad = .longPressDragging
                bridge.mouseMove(cursorNorm.x, cursorNorm.y)
                bridge.mouseButton(0, down: true)
                leftDown = true
                prevSinglePos = p
                prevMoveMs = now
                applyTrackpadDelta(p.x - startPos.x, p.y - startPos.y, dtMs: CGFloat(max(now - downMs, 1)))
            }
        case .dragging, .longPressDragging:
            let dx = p.x - prevSinglePos.x, dy = p.y - prevSinglePos.y
            let dt = CGFloat(max(now - prevMoveMs, 1))
            prevSinglePos = p
            prevMoveMs = now
            applyTrackpadDelta(dx, dy, dtMs: dt)
        }
    }

    private func finishPad() {
        let up = Self.nowMs()
        switch pad {
        case .pending:
            ripple(at: toScreen(cursorNorm) ?? startPos, right: false)
            bridge.mouseMove(cursorNorm.x, cursorNorm.y)
            click(0)
            lastTapUpMs = up
            lastTapUpPos = startPos
            return
        case .move:
            mouseMoveThrottled(cursorNorm, force: true)
            startFling(upMs: up)
        case .longPressed:
            bridge.mouseMove(cursorNorm.x, cursorNorm.y)
            click(1)
        case .dragging, .longPressDragging:
            if leftDown {
                mouseMoveThrottled(cursorNorm, force: true)
                releaseLeft()
            }
        case .twoPending:
            if up - multiDownMs <= 270 {
                mediumHaptic.impactOccurred()
                ripple(at: toScreen(cursorNorm) ?? multiStartCentroid, right: true)
                bridge.mouseMove(cursorNorm.x, cursorNorm.y)
                click(1)
            }
        case .threePending:
            if up - multiDownMs <= 280 {
                lightHaptic.impactOccurred()
                onToggleKeyboard()
            }
        case .twoPinch, .done:
            break
        }
        lastTapUpMs = 0
    }

    /// A quick flick keeps the pointer gliding for a moment.
    private func startFling(upMs: Double) {
        guard upMs - prevMoveMs <= 60 else { return }
        guard hypot(velocity.x, velocity.y) >= 0.72 else { return }
        flingVelocity = CGPoint(x: velocity.x * 12, y: velocity.y * 12)
        flingStepsLeft = 12
        flingTimer?.invalidate()
        flingTimer = Timer.scheduledTimer(timeInterval: 0.012, target: self, selector: #selector(flingTick), userInfo: nil, repeats: true)
    }

    @objc private func flingTick() {
        let v = flingVelocity
        guard flingStepsLeft > 0, hypot(v.x, v.y) >= 1.5 else {
            flingTimer?.invalidate()
            flingTimer = nil
            return
        }
        flingStepsLeft -= 1
        applyTrackpadDelta(v.x, v.y, dtMs: 12)
        flingVelocity = CGPoint(x: v.x * 0.8, y: v.y * 0.8)
    }

    @objc private func edgeTick() {
        if edgeAutoPan(lastDragPos) { mouseTo(lastDragPos) }
    }

    // MARK: - Direct mode

    private func mouseTo(_ p: CGPoint) {
        if let n = toStream(p) { bridge.mouseMove(n.x, n.y) }
    }

    private func stepDirect() {
        let pts = points()
        if direct == .hold || direct == .holdDrag {
            // Hold modes belong to the first finger; extra fingers are ignored.
            guard let f = active.first?.location(in: self) else { return }
            if direct == .hold && Self.dist(f, startPos) > touchSlop * 0.35 {
                mouseTo(startPos)
                bridge.mouseButton(0, down: true)
                direct = .holdDrag
                edgeTimer?.invalidate()
                edgeTimer = Timer.scheduledTimer(timeInterval: 0.016, target: self, selector: #selector(edgeTick), userInfo: nil, repeats: true)
            }
            if direct == .holdDrag && f != lastDragPos {
                lastDragPos = f
                mouseTo(f)
            }
            return
        }
        if pts.count >= 3 {
            if direct != .three && direct != .done {
                direct = .three
                multiDownMs = Self.nowMs()
            }
            return
        }
        if pts.count == 2 {
            let c = CGPoint(x: (pts[0].x + pts[1].x) * 0.5, y: (pts[0].y + pts[1].y) * 0.5)
            let span = Self.dist(pts[0], pts[1])
            switch direct {
            case .pending, .pan:
                direct = .twoPinch
                prevCentroid = c
                prevSpan = span
            case .twoPinch:
                applyPinchPan(centroid: c, prev: prevCentroid, span: span, prevSpan: prevSpan, recenterCursor: false)
                prevCentroid = c
                prevSpan = span
            default:
                break
            }
            return
        }
        guard let p = pts.first else { return }
        switch direct {
        case .pending:
            if Self.dist(p, startPos) > touchSlop {
                direct = .pan
                prevSinglePos = p
            }
        case .pan:
            applyPinchPan(centroid: p, prev: prevSinglePos, span: 1, prevSpan: 1, recenterCursor: false)
            prevSinglePos = p
        case .twoPinch:
            direct = .done
        default:
            break
        }
    }

    private func finishDirect() {
        edgeTimer?.invalidate()
        edgeTimer = nil
        switch direct {
        case .pending:
            if let n = toStream(startPos) {
                lastTapNorm = n
                bridge.touch(id: 0, phase: 0, n.x, n.y)
                let b = bridge
                DispatchQueue.global(qos: .userInteractive).asyncAfter(deadline: .now() + 0.025) {
                    b.touch(id: 0, phase: 2, n.x, n.y)
                }
            }
        case .hold:
            mouseTo(startPos)
            click(1)
        case .holdDrag:
            mouseTo(lastDragPos)
            bridge.mouseButton(0, down: false)
        case .three:
            if Self.nowMs() - multiDownMs <= 300 {
                lightHaptic.impactOccurred()
                onToggleKeyboard()
            }
        default:
            break
        }
        direct = .done
    }

    // MARK: - Apple Pencil (pen input with pressure, both modes)

    private func penPressure(_ t: UITouch) -> CGFloat {
        t.maximumPossibleForce > 0 ? t.force / t.maximumPossibleForce : 0.5
    }

    private func penBegan(_ t: UITouch) {
        guard penTouch == nil, let n = toStream(t.location(in: self)) else { return }
        penTouch = t
        bridge.pen(phase: 1, n.x, n.y, pressure: penPressure(t))
    }

    private func penMoved(_ t: UITouch) {
        guard let n = toStream(t.location(in: self)) else { return }
        bridge.pen(phase: 2, n.x, n.y, pressure: penPressure(t))
    }

    private func penEnded(_ t: UITouch) {
        let n = toStream(t.location(in: self)) ?? CGPoint(x: 0.5, y: 0.5)
        bridge.pen(phase: 3, n.x, n.y, pressure: 0)
        penTouch = nil
    }

    // MARK: - iPad trackpad / mouse (absolute pointer, real buttons, wheel)

    private func pointerAt(_ p: CGPoint) {
        guard let n = toStream(p) else { return }
        cursorNorm = n
        mouseMoveThrottled(n)
        updateCursor()
    }

    private func pointerBegan(_ t: UITouch, event: UIEvent?) {
        guard pointerTouch == nil else { return }
        pointerTouch = t
        let p = t.location(in: self)
        if let n = toStream(p) {
            cursorNorm = n
            bridge.mouseMove(n.x, n.y)
            if touchMode == .direct { lastTapNorm = n }
        }
        pointerButton = event?.buttonMask.contains(.secondary) == true ? 1 : 0
        bridge.mouseButton(pointerButton, down: true)
        updateCursor()
    }

    private func pointerMoved(_ t: UITouch) {
        pointerAt(t.location(in: self))
    }

    private func pointerEnded() {
        mouseMoveThrottled(cursorNorm, force: true)
        bridge.mouseButton(pointerButton, down: false)
        pointerTouch = nil
    }

    @objc private func onHover(_ g: UIHoverGestureRecognizer) {
        switch g.state {
        case .began, .changed: pointerAt(g.location(in: self))
        default: break
        }
    }

    @objc private func onScroll(_ g: UIPanGestureRecognizer) {
        switch g.state {
        case .began:
            scrollRemainder = .zero
            prepareWheel()
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            // About 40 points of two-finger travel per wheel notch (120).
            scrollRemainder.x += t.x * 3
            scrollRemainder.y += t.y * 3
            let vy = Int(scrollRemainder.y)
            if vy != 0 {
                bridge.wheel(vy, horizontal: false)
                scrollRemainder.y -= CGFloat(vy)
            }
            let vx = Int(scrollRemainder.x)
            if vx != 0 {
                bridge.wheel(-vx, horizontal: true)
                scrollRemainder.x -= CGFloat(vx)
            }
        default:
            break
        }
    }

    // MARK: - Hardware keyboard

    override var canBecomeFirstResponder: Bool { true }

    private static func scan(_ p: UIPress) -> Int {
        guard let k = p.key else { return 0 }
        return Scan.forHIDUsage(k.keyCode.rawValue)
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil { _ = becomeFirstResponder() }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var unhandled = Set<UIPress>()
        for p in presses {
            let s = Self.scan(p)
            if s != 0 {
                bridge.key(s, down: true)
            } else {
                unhandled.insert(p)
            }
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keysUp(presses, event: event, cancelled: false)
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keysUp(presses, event: event, cancelled: true)
    }

    private func keysUp(_ presses: Set<UIPress>, event: UIPressesEvent?, cancelled: Bool) {
        var unhandled = Set<UIPress>()
        for p in presses {
            let s = Self.scan(p)
            if s != 0 {
                bridge.key(s, down: false)
            } else {
                unhandled.insert(p)
            }
        }
        if unhandled.isEmpty { return }
        if cancelled { super.pressesCancelled(unhandled, with: event) } else { super.pressesEnded(unhandled, with: event) }
    }
}

/// SwiftUI host for [RemoteScreenView].
struct RemoteScreen: UIViewRepresentable {
    @ObservedObject var model: AppModel
    let controller: ScreenController
    let onToggleKeyboard: () -> Void

    func makeUIView(context: Context) -> RemoteScreenView {
        let v = RemoteScreenView(videoLayer: RcBridge.shared.video.displayLayer)
        v.controller = controller
        controller.view = v
        return v
    }

    func updateUIView(_ v: RemoteScreenView, context: Context) {
        v.onToggleKeyboard = onToggleKeyboard
        v.streamSize = model.streamSize
        if let m = model.monitors.first(where: { $0.index == model.currentMonitor }) {
            v.monitorSize = CGSize(width: m.width, height: m.height)
        } else {
            v.monitorSize = nil
        }
        v.cursorMetaKind = model.cursorMetaKind
        v.cursorVisible = model.cursorVisible
        v.touchMode = model.touchMode
        if v.cursorGlyph?.image !== model.cursor?.image {
            v.cursorGlyph = model.cursor
        }
    }
}
