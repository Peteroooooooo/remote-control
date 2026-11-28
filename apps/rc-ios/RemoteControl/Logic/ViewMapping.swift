import CoreGraphics

/// Geometry shared by the video layer, the cursor overlay and touch input. A port of the
/// Android helpers in FullscreenScreen.kt (computeVideoRect / mapScreenToStream / …) so both
/// apps place the picture and map fingers identically.
///
/// `container` is the view size, `stream` the decoded picture size in pixels, `scale` the local
/// pinch zoom (1 = fit) and `pan` the offset of the picture centre from the view centre.
enum ViewMapping {
    /// Size of the picture fitted into the container at zoom 1 (aspect kept).
    static func fittedSize(container: CGSize, stream: CGSize) -> CGSize? {
        guard container.width > 0, container.height > 0 else { return nil }
        let sw = max(stream.width, 1)
        let sh = max(stream.height, 1)
        let streamAspect = min(max(sw / sh, 0.25), 4.0)
        let viewAspect = container.width / container.height
        if viewAspect > streamAspect {
            return CGSize(width: container.height * streamAspect, height: container.height)
        }
        return CGSize(width: container.width, height: container.width / streamAspect)
    }

    /// Where the picture is drawn in view coordinates.
    static func videoFrame(container: CGSize, stream: CGSize, scale: CGFloat, pan: CGPoint) -> CGRect? {
        guard let base = fittedSize(container: container, stream: stream) else { return nil }
        let w = base.width * scale
        let h = base.height * scale
        return CGRect(
            x: container.width * 0.5 + pan.x - w * 0.5,
            y: container.height * 0.5 + pan.y - h * 0.5,
            width: w,
            height: h
        )
    }

    /// View point to normalized picture coordinates (0…1, clamped).
    static func screenToStream(_ p: CGPoint, container: CGSize, stream: CGSize, scale: CGFloat, pan: CGPoint) -> CGPoint? {
        guard let f = videoFrame(container: container, stream: stream, scale: scale, pan: pan),
              f.width > 0, f.height > 0 else { return nil }
        return CGPoint(
            x: clamp01((p.x - f.minX) / f.width),
            y: clamp01((p.y - f.minY) / f.height)
        )
    }

    /// Normalized picture coordinates to a view point.
    static func streamToScreen(_ n: CGPoint, container: CGSize, stream: CGSize, scale: CGFloat, pan: CGPoint) -> CGPoint? {
        guard let f = videoFrame(container: container, stream: stream, scale: scale, pan: pan) else { return nil }
        return CGPoint(x: f.minX + clamp01(n.x) * f.width, y: f.minY + clamp01(n.y) * f.height)
    }

    /// Keeps a zoomed picture covering the view: no empty band can be dragged in.
    static func clampPan(_ pan: CGPoint, scale: CGFloat, container: CGSize, stream: CGSize) -> CGPoint {
        guard let base = fittedSize(container: container, stream: stream) else { return .zero }
        let maxX = max(base.width * scale - container.width, 0) * 0.5
        let maxY = max(base.height * scale - container.height, 0) * 0.5
        return CGPoint(x: min(max(pan.x, -maxX), maxX), y: min(max(pan.y, -maxY), maxY))
    }

    /// View points per host desktop pixel, so the cursor image is drawn as large as on the PC.
    /// Uses the monitor width when its aspect matches the stream, else the stream width.
    static func hostToScreenScale(container: CGSize, stream: CGSize, monitor: CGSize?, zoom: CGFloat) -> CGFloat {
        guard let base = fittedSize(container: container, stream: stream) else { return 1 }
        let streamW = max(stream.width, 1)
        let streamAspect = streamW / max(stream.height, 1)
        var desktopW = streamW
        if let m = monitor, m.width > 0, m.height > 0,
           abs(m.width / m.height - streamAspect) <= streamAspect * 0.03 {
            desktopW = m.width
        }
        return max(base.width * zoom / desktopW, 0.01)
    }

    static func clamp01(_ v: CGFloat) -> CGFloat { min(max(v, 0), 1) }
}

/// Pointer acceleration for trackpad mode (same curve as Android): slow moves are precise
/// (0.45×), medium ones comfortable (~1.15×), fast flicks cover the screen (up to 2.45×).
enum TrackpadCurve {
    /// `speed` in points per millisecond; `overInteractive` when the host cursor is a hand,
    /// resize or text cursor, which slows slow moves a little to make targets easier to hit.
    static func gain(speed: CGFloat, overInteractive: Bool) -> CGFloat {
        var g: CGFloat
        if speed < 0.16 {
            g = 0.45 + (speed / 0.16) * 0.35
        } else if speed < 0.55 {
            g = 0.80 + ((speed - 0.16) / 0.39) * 0.65
        } else {
            g = min(1.45 + (speed - 0.55) * 1.15, 2.45)
        }
        if overInteractive && speed < 0.28 {
            g *= 0.78
        }
        return g
    }
}
