# NTE patch notes

Vendored copy of `egui-winit` 0.34.3 (unmodified upstream source from crates.io,
license `MIT OR Apache-2.0`, see `LICENSE-MIT` / `LICENSE-APACHE`), with one
patch in `src/lib.rs`'s `create_winit_window_attributes`: transparent windows
on Windows now get `WindowAttributesExtWindows::with_no_redirection_bitmap(true)`.

Without it, transparent windows keep their default GDI redirection surface,
which prevents DXGI/DirectComposition swapchains from ever reporting a
`CompositeAlphaMode` other than `Opaque` — a "transparent" window (e.g. this
app's HUD overlay) painted solid black. This is paired with
`wgpu::Dx12SwapchainKind::DxgiFromVisual` set in `src/main.rs`, which routes
the DX12 swapchain through a `DirectComposition` visual that can actually
composite alpha with the desktop; without also skipping the redirection
bitmap here, that visual's window still had its own default white background
and native title bar showing through around it.

Upstream doesn't expose this Windows-only winit attribute through any public
eframe/egui-winit API (checked against egui's unreleased `main` branch too, as
of 2026-07), so this fork exists until upstream adds a hook. See
`Cargo.toml`'s `[patch.crates-io]` comment in the project root.

Re-diff this patch against a fresh egui-winit release before bumping the
`eframe`/`egui-winit` version pin.
