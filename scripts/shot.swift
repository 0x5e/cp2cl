// Renders the guide page to a PNG of an exact pixel size, using the WebKit that
// ships with macOS. Called by `scripts/guide.sh` once per screen size.
//
//     swift scripts/shot.swift <page.html> <width> <height> <out.png>
//
// The page is laid out at the size asked for, so one pixel of CSS is one pixel
// of the stream that gets encoded from it; the snapshot comes back at the
// display's scale and is drawn back down to that.
import Cocoa
import WebKit

final class Shot: NSObject, WKNavigationDelegate {
    private let size: CGSize
    private let out: URL
    private let web: WKWebView
    private let window: NSWindow

    init(size: CGSize, out: URL) {
        self.size = size
        self.out = out
        self.web = WKWebView(frame: CGRect(origin: .zero, size: size))
        self.window = NSWindow(contentRect: CGRect(origin: .zero, size: size),
                               styleMask: [.borderless], backing: .buffered,
                               defer: false)
        super.init()
        web.navigationDelegate = self
        window.contentView = web
        window.orderBack(nil)
    }

    func load(_ page: URL) {
        web.loadFileURL(page, allowingReadAccessTo: page.deletingLastPathComponent())
    }

    // A page that has loaded is not yet a page that has been painted; the delay
    // is what the layout and the fonts need before the snapshot is taken.
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { self.snap() }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError: Error) {
        fail("load failed: \(withError)")
    }

    private func snap() {
        let config = WKSnapshotConfiguration()
        config.rect = CGRect(origin: .zero, size: size)
        config.snapshotWidth = NSNumber(value: Double(size.width))
        web.takeSnapshot(with: config) { image, error in
            guard let image = image else { self.fail("snapshot failed: \(String(describing: error))") }
            guard let pixels = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(self.size.width),
                pixelsHigh: Int(self.size.height), bitsPerSample: 8,
                samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
            else { self.fail("no bitmap") }

            NSGraphicsContext.saveGraphicsState()
            NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: pixels)
            NSGraphicsContext.current?.imageInterpolation = .high
            image.draw(in: NSRect(origin: .zero, size: self.size))
            NSGraphicsContext.restoreGraphicsState()

            guard let png = pixels.representation(using: .png, properties: [:]) else {
                self.fail("no png")
            }
            do { try png.write(to: self.out) } catch { self.fail("\(error)") }
            print("\(self.out.lastPathComponent) \(pixels.pixelsWide)x\(pixels.pixelsHigh)")
            exit(0)
        }
    }

    private func fail(_ why: String) -> Never {
        FileHandle.standardError.write("shot: \(why)\n".data(using: .utf8)!)
        exit(1)
    }
}

let args = CommandLine.arguments
guard args.count == 5, let width = Int(args[2]), let height = Int(args[3]) else {
    FileHandle.standardError.write("usage: shot <page.html> <width> <height> <out.png>\n".data(using: .utf8)!)
    exit(2)
}

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

let shot = Shot(size: CGSize(width: width, height: height),
                out: URL(fileURLWithPath: args[4]))
shot.load(URL(fileURLWithPath: args[1]))

// A page that never finishes loading would otherwise hang the build.
DispatchQueue.main.asyncAfter(deadline: .now() + 20) {
    FileHandle.standardError.write("shot: timed out\n".data(using: .utf8)!)
    exit(1)
}
app.run()
