# Studio window activation hook

`carbon_studio_window_guard_hook.dll` is the deterministic x86_64 Windows build
of `studio_window_guard_hook.rs`. The PowerShell window guardian loads this tiny
CBT hook into only the verified Studio UI threads while that Studio is parked.

Rebuild it from the repository root with the pinned Rust toolchain:

```bash
rustup target add x86_64-pc-windows-msvc
rust_lld="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"
rustc cli/native/studio_window_guard_hook.rs \
  --edition 2021 \
  --crate-name carbon_studio_window_guard_hook \
  --crate-type cdylib \
  --target x86_64-pc-windows-msvc \
  -C panic=abort \
  -C opt-level=z \
  -C strip=symbols \
  -C linker="$rust_lld" \
  -C link-arg=/nodefaultlib \
  -C link-arg=/entry:DllMain \
  -C link-arg=/DEBUG:NONE \
  -C link-arg=/timestamp:0 \
  -o cli/native/carbon_studio_window_guard_hook.dll
```

The expected SHA-256 digest is
`77b94e0bffeee66572e8cf64533723787369cce5699283717f5cb0bff9ff5017`.
