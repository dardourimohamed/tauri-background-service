// swift-tools-version:5.5
// Copyright 2019-2024 Sila. Tauri iOS plugin manifest for the background-service plugin.
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import PackageDescription

// `tauri_utils::build::link_apple_library` routes to swift-rs (`link_swift_library`)
// only when this manifest exists; without it the build falls through to
// `link_xcode_library` and fails ("ios does not contain an Xcode project ...").
// swift-rs `SwiftLinker::with_package(name, path)` requires `name` (the crate's
// CARGO_PKG_NAME) to match this package's `name` field, and links
// `static=<name>` — so the package, product, and target all share this name.
let package = Package(
  name: "tauri-plugin-background-service",
  platforms: [
    .iOS(.v14),
  ],
  products: [
    .library(
      name: "tauri-plugin-background-service",
      type: .static,
      targets: ["tauri-plugin-background-service"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-background-service",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources/TauriPluginBackgroundService"),
    // Real iOS-Simulator XCTest target (H12). `swift test` on macOS stays broken by
    // the UIKit/BackgroundTasks dependency and is a documented non-gate; the Swift
    // behavior gate is `xcodebuild test -scheme tauri-plugin-background-service
    // -destination 'platform=iOS Simulator,name=iPhone 17 Pro'`, which builds for the
    // simulator where those frameworks exist. The target name's dashes sanitize to a
    // `tauri_plugin_background_service` Swift module, so tests `@testable import`
    // that underscored name.
    .testTarget(
      name: "tauri-plugin-background-serviceTests",
      dependencies: [
        .byName(name: "tauri-plugin-background-service")
      ],
      path: "Tests/TauriPluginBackgroundServiceTests")
  ]
)
