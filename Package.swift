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
            name: "ZoraCore",
            linkerSettings: [.linkedLibrary("sqlite3")]
        ),
        .target(
            name: "ZoraMac",
            dependencies: ["ZoraCore"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .executableTarget(
            name: "zora",
            dependencies: ["ZoraCore", "ZoraMac"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "ZoraCoreTests",
            dependencies: ["ZoraCore"]
        ),
    ]
)
