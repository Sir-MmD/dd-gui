# Vendored crates

## i-slint-backend-winit 1.18.1

An unmodified copy of Slint's winit backend from crates.io, except for one change in
`frame_throttle.rs` (marked "DD-GUI patch"). `Cargo.toml` swaps it in with `[patch.crates-io]`.

- **The bug:** Slint computes the frame interval as `1_000_000 / refresh_rate_millihertz`.
  Windows reports 0 or 1 Hz as a monitor's refresh rate when it uses "the hardware's
  default rate", which happens in some VMs and remote sessions. That makes the division
  panic, or the UI animate at 1 frame per second. It also shows up under Wine with XRandR.
- **The fix:** rates under 10 Hz count as unknown, so Slint falls back to its 60 Hz default.

Slint's license files are kept in `LICENSES/`; the royalty-free license allows modified
copies distributed as part of an application. When updating Slint, drop this copy, or
refresh it from the new version and re-apply the patch if upstream hasn't fixed it.
