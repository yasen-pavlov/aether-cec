#!/usr/bin/env bash
# Check a finished `cargo build --release` of firmware/ and package its images.
#
#   scripts/package-firmware.sh <output-dir>
#
# Fails when the generated sdkconfig does not carry every line of
# sdkconfig.defaults (embuild can silently skip the defaults) or when the
# application does not fit an OTA slot. Writes:
#   aether-ota.bin      application image for POST /api/ota
#   aether-factory.bin  bootloader + partition table + application, flash at 0x0
#   aether.elf          symbols for crash symbolisation
#   SHA256SUMS
set -euo pipefail

out_dir=${1:?usage: package-firmware.sh <output-dir>}
firmware=$(cd "$(dirname "$0")/../firmware" && pwd)
release=$firmware/target/xtensa-esp32-espidf/release
elf=$release/aether
test -f "$elf" || { echo "missing $elf: run cargo build --release in firmware/ first" >&2; exit 1; }

# The esp-idf-sys output of the build that produced the ELF: newest sdkconfig.
sdkconfig=$(ls -t "$release"/build/esp-idf-sys-*/out/sdkconfig | head -n 1)
idf_out=$(dirname "$sdkconfig")
bootloader=$idf_out/build/bootloader/bootloader.bin
test -f "$bootloader" || { echo "missing $bootloader" >&2; exit 1; }

missing=0
while IFS= read -r line; do
  case $line in
    CONFIG_*=* | '# CONFIG_'*' is not set')
      if ! grep -qxF -- "$line" "$sdkconfig"; then
        echo "sdkconfig.defaults line not applied: $line" >&2
        missing=1
      fi
      ;;
  esac
done < "$firmware/sdkconfig.defaults"
test "$missing" -eq 0 || { echo "generated sdkconfig: $sdkconfig" >&2; exit 1; }

mkdir -p "$out_dir"
espflash save-image --chip esp32 "$elf" "$out_dir/aether-ota.bin"
espflash save-image --chip esp32 --merge --flash-size 4mb \
  --partition-table "$firmware/partitions.csv" --bootloader "$bootloader" \
  "$elf" "$out_dir/aether-factory.bin"
cp "$elf" "$out_dir/aether.elf"

slot=$(awk -F, '$1 == "ota_0" { gsub(/ /, "", $5); print $5 }' "$firmware/partitions.csv")
slot=$((slot))
size=$(stat -c %s "$out_dir/aether-ota.bin")
if [ "$size" -gt "$slot" ]; then
  echo "application is $size bytes, OTA slot is $slot" >&2
  exit 1
fi

(cd "$out_dir" && sha256sum aether-ota.bin aether-factory.bin aether.elf > SHA256SUMS)
echo "application $size bytes of $slot ($((100 * size / slot)) %), $((slot - size)) free"
