// swift-tools-version:6.0
import PackageDescription

// Parakeet ANE sidecar: runs Parakeet via FluidAudio's CoreML build on the
// Apple Neural Engine, speaking the SAME length-prefixed stdin/stdout protocol
// as the Rust ONNX sidecar (crates/parakeet-sidecar) so the app's ParakeetClient
// talks to either binary unchanged. Apple Silicon only.
let package = Package(
    name: "parakeet-ane-sidecar",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/FluidInference/FluidAudio.git", from: "0.12.4"),
    ],
    targets: [
        .executableTarget(
            name: "parakeet-ane-sidecar",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")]
        ),
    ]
)
