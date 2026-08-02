# Ironmesh Android (initial MVP)

This is the IronMesh Android app.

## Features (MVP)

- Enroll with a connection-bootstrap claim or import a connection-bootstrap bundle
- Connect through the Rust `client-sdk` using the routes advertised by that bundle
- Upload/download via the Rust `client-sdk` bridge
- Open the client Web UI in a browser-powered Custom Tab when available, with fallback to an in-app `WebView`
- Configure multiple folder-sync profiles (remote prefix <-> local folder)
- Automatic periodic background folder sync (WorkManager) + manual "Sync Now"
- Optional Rust-backed title latency monitor with configurable period; compact `D` (direct) or `R` (relay) result in the app bar
- App-bar diagnostic export that writes retained Android application events (including sanitized embedded WebView/JavaScript failures), current connection state, and Rust tracing output to a user-selected text file

## Open in Android Studio

Open this folder as a project:

- `apps/android-app`

Android Studio will sync Gradle and let you run the `app` module.

Rust JNI integration is wired into the app Gradle build:

- `preBuild` runs `cargo ndk ... build` for Android ABIs
- generated `.so` files are packaged from the variant-specific
  `app/build/generated/rustJniLibs/debug` or `app/build/generated/rustJniLibs/release` directory
- JNI load name is `android_app` (`System.loadLibrary("android_app")`)

Prerequisites for native bridge builds:

- Rust toolchain installed
- `cargo-ndk` installed (`cargo install cargo-ndk`)
- Android NDK available in the Android SDK setup

## Internal release signing

`assembleRelease` uses a dedicated internal release key when these environment variables are set:

- `IRONMESH_ANDROID_INTERNAL_RELEASE_STORE_FILE`
- `IRONMESH_ANDROID_INTERNAL_RELEASE_STORE_PASSWORD`
- `IRONMESH_ANDROID_INTERNAL_RELEASE_KEY_ALIAS`
- `IRONMESH_ANDROID_INTERNAL_RELEASE_KEY_PASSWORD`

In GitHub Actions, store the keystore itself as base64 in `IRONMESH_ANDROID_INTERNAL_RELEASE_STORE_B64`, decode it to a file, then export `IRONMESH_ANDROID_INTERNAL_RELEASE_STORE_FILE` for Gradle before running `:app:assembleRelease`.

## Rust bridge notes

- JNI bridge class: `io.ironmesh.android.data.RustClientBridge`
- Rust exports implemented in: `apps/android-app/src/lib.rs`
- `IronmeshApplication` initializes the process-wide Rust bridges with the
  Android application context before Iroh can construct its system DNS resolver.
- Current Rust-backed operations in repository:
  - `putObject`, `putObjectBytes`
  - `getObject`, `getObjectBytes` (latest-only path; snapshot/version still uses HTTP fallback)
  - `startWebUi` (starts embedded local web UI server and returns localhost URL)
