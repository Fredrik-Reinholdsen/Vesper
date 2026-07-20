#!/bin/sh

has_rp2350_bootloader() {
  # Returns 0 (true) if 2e8a:000f is present, 1 (false) otherwise
  if lsusb | grep -qi '2e8a:000f'; then
    return 0
  else
    return 1
  fi
}

# Example usage:
if has_rp2350_bootloader; then
  echo "RP2350 USBbootloader is connected! Flashing over USB..."
  elf2uf2-rs deploy -t --family rp2350-arm-s $1
else
  echo "No RP2350 bootloader not detected, running DAP"
  probe-rs run --chip RP235x $1
fi
