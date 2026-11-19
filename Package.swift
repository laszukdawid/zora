// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Zora",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "zora", targets: ["zora"]),
    ],
    targets: [
        .target(
            name: "ZoraMac",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .executableTarget(
            name: "zora",
            dependencies: ["ZoraMac"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
