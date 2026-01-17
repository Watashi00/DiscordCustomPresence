# Custom Rich Presence

Two apps coexist:
- Tauri UI (web-based UI)
- Native Rust UI (lightweight, performance-focused)

Prerequisites
- Rust toolchain (stable)
- C toolchain
  - Windows: MSVC Build Tools
  - Linux: `build-essential` or equivalent
- Bun (for the Tauri web UI build)
- Node.js (optional; only if you prefer npm)

Dependencies
- Tauri app: `src-tauri/Cargo.toml`
- Native app: `native/Cargo.toml`
- Shared RPC core: `crates/rpc-core/Cargo.toml`

Development
- Tauri dev: `bun run tauri:dev`
- Native dev: `cargo run -p custom_rich_presence_native`

Builds
- Tauri: `bun run tauri:build`
- Native (Linux/Windows): `cargo build -p custom_rich_presence_native --release`

Notes
- The native app stores config in a local `config.json` under your OS config directory.
