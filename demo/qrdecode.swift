import Foundation
import Vision

// Reads the payload out of a QR in an image, so a code shown on a phone can be
// consumed by a host that has no camera.
let path = CommandLine.arguments[1]
guard let image = NSData(contentsOfFile: path) as Data? else { exit(2) }
let request = VNDetectBarcodesRequest()
request.symbologies = [.qr]
let handler = VNImageRequestHandler(data: image, options: [:])
try handler.perform([request])
for observation in request.results ?? [] {
    if let payload = observation.payloadStringValue { print(payload) }
}
