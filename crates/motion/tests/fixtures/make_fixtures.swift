// Writes the test videos: `swift make_fixtures.swift OUT WIDTH HEIGHT FRAMES FPS [rotate] [hevc]`.
//
// Frame `n`'s whole picture is one colour, its red channel counting up, so a test can tell which frame it got. Past 256 pixels wide
// a bright square also moves across, so a real-window run has something to look at.
import AVFoundation
import CoreVideo

let arguments = CommandLine.arguments
let out = URL(fileURLWithPath: arguments[1])
let width = Int(arguments[2])!
let height = Int(arguments[3])!
let frames = Int(arguments[4])!
let fps = Int32(arguments[5])!
let rotate = arguments.contains("rotate")
let hevc = arguments.contains("hevc")

try? FileManager.default.removeItem(at: out)
let writer = try! AVAssetWriter(outputURL: out, fileType: .mp4)
let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
    AVVideoCodecKey: hevc ? AVVideoCodecType.hevc : AVVideoCodecType.h264,
    AVVideoWidthKey: width,
    AVVideoHeightKey: height,
    AVVideoCompressionPropertiesKey: [AVVideoAverageBitRateKey: max(width * height * 6, 200_000)],
])
input.expectsMediaDataInRealTime = false
if rotate {
    input.transform = CGAffineTransform(a: 0, b: 1, c: -1, d: 0, tx: CGFloat(height), ty: 0)
}
let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: [
    kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
    kCVPixelBufferWidthKey as String: width,
    kCVPixelBufferHeightKey as String: height,
])
writer.add(input)
writer.startWriting()
writer.startSession(atSourceTime: .zero)

for n in 0..<frames {
    while !input.isReadyForMoreMediaData {
        Thread.sleep(forTimeInterval: 0.001)
    }
    var buffer: CVPixelBuffer?
    CVPixelBufferPoolCreatePixelBuffer(nil, adaptor.pixelBufferPool!, &buffer)
    let pixels = buffer!
    CVPixelBufferLockBaseAddress(pixels, [])
    let base = CVPixelBufferGetBaseAddress(pixels)!.assumingMemoryBound(to: UInt8.self)
    let stride = CVPixelBufferGetBytesPerRow(pixels)
    let red = UInt8(frames > 1 ? n * 255 / (frames - 1) : 0)
    let square = width > 256 ? (n * 8 % max(width - 240, 1), (n * 5) % max(height - 240, 1)) : (-1, -1)
    for y in 0..<height {
        let row = base + y * stride
        for x in 0..<width {
            let bright = square.0 >= 0 && x >= square.0 && x < square.0 + 240 && y >= square.1 && y < square.1 + 240
            row[x * 4] = bright ? 255 : UInt8(y * 200 / height + 30)
            row[x * 4 + 1] = bright ? 255 : UInt8(x * 200 / width + 30)
            row[x * 4 + 2] = bright ? 255 : red
            row[x * 4 + 3] = 255
        }
    }
    CVPixelBufferUnlockBaseAddress(pixels, [])
    adaptor.append(pixels, withPresentationTime: CMTime(value: CMTimeValue(n), timescale: fps))
}
input.markAsFinished()
let done = DispatchSemaphore(value: 0)
writer.finishWriting { done.signal() }
done.wait()
if writer.status != .completed {
    print("failed: \(String(describing: writer.error))")
    exit(1)
}
print("wrote \(out.path)")
