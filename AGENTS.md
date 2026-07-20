# AGENTS.md

## Project

Vesper — split keyboard firmware for RP2350 (ARM Cortex-M33). Uses Embassy async framework. `#![no_std]`, `#![no_main]`, Rust 2024 edition.

## Workspace layout

| Crate | Purpose |
|-------|---------|
| `master/` | Master board: USB HID, keyboard scanning, LED control, slave bootloader via UART |
| `slave/` | Slave board: button scanning, LED control, UART comms with master |
| `common/` | Shared types: `button_array`, `command` (postcard-serialized `SlaveEvent`) |

Non-Rust: `electronics/` (KiCad), `mechanics/` (FreeCAD/STL).

## Build

Target is hardcoded in `.cargo/config.toml`: `thumbv8m.main-none-eabihf`.

**Build order matters.** `master/build.rs` embeds the slave binary via `rust-objcopy`. You must build slave first:

```
cargo build -p slave --release
cargo build -p master --release
```

Building `master` without a prior slave build will panic in `build.rs`.

## Flash / run

Runner is `probe-rs run --chip RP235x` (configured in `.cargo/config.toml`). Logging via `defmt-rtt` at `DEFMT_LOG=debug`.

## Memory layout

- Master: flash at `0x10000000`, 2 MiB
- Slave: flash at `0x10000000 + 32K` (master reserves first 32K for the slave bootloader)
- Both: 512K RAM at `0x20000000`

## Key conventions

- All workspace deps are declared in root `Cargo.toml` — crates use `{ workspace = true }`
- Communication between master and slave uses `postcard` serialization over UART
- LED drivers: WS2812 via PIO (`PioWs2812`)
- Motor driver: `tb6612fng` (listed as dep, used for something — check before removing)
- `static_cell::StaticCell` is used for USB/HID buffer allocation (required by Embassy patterns)
- `assign-resources` (git dep) is used for peripheral splitting
