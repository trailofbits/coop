// swift-tools-version: 6.2
// coop-sandbox: coop's macOS VM runtime, built directly on apple/containerization.
// Pinned exactly: the package's API changes between minor releases.
import PackageDescription

let package = Package(
    name: "coop-sandbox",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "coop-sandbox", targets: ["CoopSandbox"])
    ],
    dependencies: [
        .package(url: "https://github.com/apple/containerization.git", exact: "0.45.0"),
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.7.0"),
        // Already resolved through containerization; named for `FilePath`.
        .package(url: "https://github.com/apple/swift-system.git", from: "1.6.4"),
    ],
    targets: [
        .target(
            name: "CoopSandboxCore",
            dependencies: [
                .product(name: "Containerization", package: "containerization"),
                .product(name: "ContainerizationExtras", package: "containerization"),
                .product(name: "ContainerizationOCI", package: "containerization"),
                .product(name: "ContainerizationEXT4", package: "containerization"),
                .product(name: "ContainerizationOS", package: "containerization"),
                .product(name: "SystemPackage", package: "swift-system"),
            ]
        ),
        .executableTarget(
            name: "CoopSandbox",
            dependencies: [
                "CoopSandboxCore",
                .product(name: "Containerization", package: "containerization"),
                .product(name: "ArgumentParser", package: "swift-argument-parser"),
            ]
        ),
        .testTarget(
            name: "CoopSandboxTests",
            dependencies: [
                "CoopSandboxCore",
                .product(name: "ContainerizationEXT4", package: "containerization"),
                .product(name: "SystemPackage", package: "swift-system"),
            ]
        ),
    ]
)
