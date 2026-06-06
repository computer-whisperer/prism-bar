# prism-bar

Status bar for the [prism](https://github.com/computer-whisperer/prism)
compositor — rendered with the
[damascene](https://github.com/computer-whisperer/damascene) UI toolkit on a
`wlr-layer-shell` surface, drawn by wgpu. Works on any compositor that
speaks layer-shell; the workspace and window-title modules light up when the
compositor also offers the matching protocols.

One bar per output (all outputs by default, hotplug included), each with its
own layer surface, swapchain, and damascene runner. The event loop is
calloop-driven and redraws only on actual change: protocol events, clock
ticks, monitor samples, and animation deadlines — no fixed frame loop.

## Modules

Left cluster:

- **prism** label
- **workspaces** — pills for active/inactive/urgent workspaces, click to
  switch (`ext-workspace-v1`; absent protocol → no pills)
- **window title** — focused toplevel's title
  (`wlr-foreign-toplevel-management`; absent protocol → no title)

Right cluster, picked and ordered by config:

- **cpu** — aggregate load gauge (`/proc/stat`)
- **memory** — usage gauge (1 − MemAvailable/MemTotal)
- **disk "/path"** — per-mount usage gauge (statvfs); repeatable, non-root
  mounts get a path label
- **clock** — strftime format, tabular digits so the layout never reflows

Gauges take per-module `hot`/`width`/`thickness`; the percent readout stacks
over its own bar.

## Configuration

KDL, at `$PRISM_BAR_CONFIG`, else `$XDG_CONFIG_HOME/prism-bar/config.kdl`,
else `~/.config/prism-bar/config.kdl`. A missing file means built-in
defaults; a file that fails to parse is a startup error with
miette-annotated diagnostics — no silent fallback over a typo.

Edits apply live (inotify, 150 ms debounce): geometry changes resize running
bars, module-list changes rebuild the clusters. A save that fails to parse
logs the error and keeps the current config.

[`resources/default-config.kdl`](resources/default-config.kdl) documents
every option with its default: `height`, `margin`, `position` (top/bottom),
`opacity`, `radius`, `theme`, `border`, `title-max-length`, `output`
pinning, `sample-interval`, and the `modules` block.

## Requirements

- Wayland compositor with `wlr-layer-shell`; `ext-workspace-v1` and
  `wlr-foreign-toplevel-management` are optional extras
- A wgpu-capable GPU (Vulkan on Linux) and system libwayland — wgpu's WSI
  needs raw `wl_display`/`wl_surface` pointers

## Building

```
cargo build --release
```

An AUR-oriented `PKGBUILD` is provided for tagged releases. It installs the
binary, this README, and the license files.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

The gauge icons under `assets/icons/` are vendored from
[Lucide](https://lucide.dev) (ISC license).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
