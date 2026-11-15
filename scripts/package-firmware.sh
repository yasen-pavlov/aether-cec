#!/usr/bin/env bash
# Build firmware/ in release mode, check the result and package its images.
#
#   [AETHER_VERSION=X.Y.Z] scripts/package-firmware.sh <output-dir>
#
# The ELF and the esp-idf-sys output directory (sdkconfig, bootloader) are the
# ones Cargo reports for this very build, never a guess among leftover build
# directories. Fails when the generated sdkconfig does not carry every line of
# sdkconfig.defaults (embuild can silently skip the defaults) or when the
# application does not fit an OTA slot. Writes:
#   aether-ota.bin      application image for POST /api/ota
#   aether-factory.bin  bootloader + partition table + application, flash at 0x0
#   aether.elf          symbols for crash symbolisation
#   SHA256SUMS
set -euo pipefail

out_dir=${1:?usage: package-firmware.sh <output-dir>}
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
firmware=$(cd "$(dirname "$0")/../firmware" && pwd)
cd "$firmware"

messages=$(mktemp)
trap 'rm -f "$messages"' EXIT
cargo build --release --locked --message-format=json-render-diagnostics > "$messages"

# Exactly one line each, or the build is not what this script assumes.
one() {
  local what=$1 value=$2
  if [[ -z $value || $(printf '%s\n' "$value" | wc -l) -ne 1 ]]; then
    echo "expected exactly one $what from cargo, got: ${value:-nothing}" >&2
    exit 1
  fi
}
elf=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "aether" and .executable != null) | .executable' "$messages")
one "aether executable" "$elf"
idf_out=$(jq -r 'select(.reason == "build-script-executed" and (.package_id | test("[#/ ]esp-idf-sys[@ ]|^esp-idf-sys "))) | .out_dir' "$messages")
one "esp-idf-sys out_dir" "$idf_out"
sdkconfig=$idf_out/sdkconfig
bootloader=$idf_out/build/bootloader/bootloader.bin
[[ -f $sdkconfig ]] || { echo "missing $sdkconfig" >&2; exit 1; }
[[ -f $bootloader ]] || { echo "missing $bootloader" >&2; exit 1; }

missing=0
while IFS= read -r line; do
  case $line in
    CONFIG_*=* | '# CONFIG_'*' is not set')
      if ! grep -qxF -- "$line" "$sdkconfig"; then
        echo "sdkconfig.defaults line not applied: $line" >&2
        missing=1
      fi
      ;;
    *) ;; # comments, blank lines
  esac
done < sdkconfig.defaults
[[ $missing -eq 0 ]] || { echo "generated sdkconfig: $sdkconfig" >&2; exit 1; }

espflash save-image --chip esp32 "$elf" "$out_dir/aether-ota.bin"
espflash save-image --chip esp32 --merge --flash-size 4mb \
  --partition-table partitions.csv --bootloader "$bootloader" \
  "$elf" "$out_dir/aether-factory.bin"
cp "$elf" "$out_dir/aether.elf"

slot=$(awk -F, '$1 == "ota_0" { gsub(/ /, "", $5); print $5 }' partitions.csv)
slot=$((slot))
size=$(stat -c %s "$out_dir/aether-ota.bin")
if [[ $size -gt $slot ]]; then
  echo "application is $size bytes, OTA slot is $slot" >&2
  exit 1
fi

(cd "$out_dir" && sha256sum aether-ota.bin aether-factory.bin aether.elf > SHA256SUMS)
echo "application $size bytes of $slot ($((100 * size / slot)) %), $((slot - size)) free"
