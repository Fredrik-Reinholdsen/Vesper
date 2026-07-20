#!/bin/sh

check_bin_size() {
  # Checks that the compiled binary is small enough to fit into flash memory (256kB)
  # If not, an error is raised and script will exit.
  MAX=$((256 * 1024))

  BIN=$(mktemp)
  trap "rm -f $BIN" EXIT
  rust-objcopy -O binary $1 "$BIN"
  SIZE=$(stat -c%s "$BIN")

  if [ "$SIZE" -gt "$MAX" ]; then
    echo "ERROR: binary is $((SIZE / 1024))kB, exceeds 256kB limit" >&2
    exit 1
  fi

  return 0
}

has_rp2350_bootloader() {
  # Returns 0 (true) if 2e8a:000f is present, 1 (false) otherwise
  if lsusb | grep -qi '2e8a:000f'; then
    return 0
  else
    return 1
  fi
}

# ---------- main ----------
check_bin_size "$1"

# Example usage:
if has_rp2350_bootloader; then
  echo "RP2350 USBbootloader is connected! Flashing over USB..."
  elf2uf2-rs deploy -t --family rp2350-arm-s "$1"
else
  echo "No RP2350 bootloader not detected, running DAP"
  probe-rs run --chip RP235x "$1"
fi
