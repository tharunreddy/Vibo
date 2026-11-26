#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="LIVI"
BUNDLE_ID="com.yada.livi"
INSTALL_PATH="/Applications/${APP_NAME}.app"
RESET_PERMISSIONS=true

for arg in "$@"; do
    case "$arg" in
        --reset-permissions) RESET_PERMISSIONS=true ;;
        --keep-permissions) RESET_PERMISSIONS=false ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

export PATH="$HOME/.cargo/bin:$PATH"

echo "==> Building ${APP_NAME} (release)..."
cd "$ROOT_DIR"
cargo build --release

BIN_PATH="$ROOT_DIR/target/release/${APP_NAME}"
if [[ ! -f "$BIN_PATH" ]]; then
    echo "Error: Release binary not found at $BIN_PATH"
    exit 1
fi

echo "==> Generating macOS app icon (${APP_NAME}.icns)..."
ICON_WORK="$(mktemp -d)"
trap 'rm -rf "$ICON_WORK"' EXIT
ICONSET="$ICON_WORK/${APP_NAME}.iconset"
mkdir -p "$ICONSET"

# Generate a clean 1024x1024 minimalist waveform icon using macOS built-in Python/CoreGraphics
# (falls back to a minimal PNG via sips if PyObjC/Quartz isn't present).
python3 - "$ICON_WORK/icon_1024.png" <<'PYEOF'
import math, struct, sys, zlib

out_path = sys.argv[1]
size = 1024
buf = bytearray(size * size * 4)

def blend(idx, r, g, b, a):
    if a <= 0:
        return
    inv = 1.0 - a
    buf[idx]     = min(255, int(r * a + buf[idx] * inv))
    buf[idx + 1] = min(255, int(g * a + buf[idx + 1] * inv))
    buf[idx + 2] = min(255, int(b * a + buf[idx + 2] * inv))
    buf[idx + 3] = min(255, int((a + (buf[idx + 3] / 255.0) * inv) * 255))

def sd_rounded_rect(px, py, cx, cy, hx, hy, rad):
    qx = abs(px - cx) - hx + rad
    qy = abs(py - cy) - hy + rad
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - rad

bars = [
    (-240, 140),
    (-120, 290),
    (   0, 420),
    ( 120, 290),
    ( 240, 140),
]

cx = cy = size / 2.0
for y in range(size):
    t = y / size
    bg_r = int(20 + 18 * t)
    bg_g = int(24 + 20 * t)
    bg_b = int(38 + 34 * t)
    for x in range(size):
        d = sd_rounded_rect(x + 0.5, y + 0.5, cx, cy, 430.0, 430.0, 196.0)
        if d > 1.5:
            continue
        idx = (y * size + x) * 4
        alpha = max(0.0, min(1.0, 0.5 - d))
        blend(idx, bg_r, bg_g, bg_b, alpha)
        # Subtle top highlight border
        if -5.0 < d < 0.0:
            blend(idx, 255, 255, 255, 0.14 * alpha)
        # Waveform pill bars
        for bx, bh in bars:
            bd = sd_rounded_rect(x + 0.5, y + 0.5, cx + bx, cy, 34.0, bh / 2.0, 34.0)
            if bd < 1.5:
                ba = max(0.0, min(1.0, 0.5 - bd)) * alpha
                # Subtle cyan-to-violet gradient across bars
                u = (bx + 240.0) / 480.0
                r = 120 + 115 * u
                g = 210 - 65 * u
                b = 255
                blend(idx, r, g, b, ba)

raw = bytearray()
stride = size * 4
for y in range(size):
    raw.append(0)
    raw.extend(buf[y * stride:(y + 1) * stride])

def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 6)) + chunk(b"IEND", b"")
with open(out_path, "wb") as f:
    f.write(png)
PYEOF

for sz in 16 32 128 256 512; do
    sips -z "$sz" "$sz" "$ICON_WORK/icon_1024.png" --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
    sz2=$((sz * 2))
    sips -z "$sz2" "$sz2" "$ICON_WORK/icon_1024.png" --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$ROOT_DIR/resources/${APP_NAME}.icns"

echo "==> Assembling ${APP_NAME}.app bundle..."
STAGE_APP="$ICON_WORK/${APP_NAME}.app"
mkdir -p "$STAGE_APP/Contents/MacOS" "$STAGE_APP/Contents/Resources"
cp "$BIN_PATH" "$STAGE_APP/Contents/MacOS/${APP_NAME}"
chmod +x "$STAGE_APP/Contents/MacOS/${APP_NAME}"
cp "$ROOT_DIR/macos/Info.plist" "$STAGE_APP/Contents/Info.plist"
cp "$ROOT_DIR/resources/"*.wav "$STAGE_APP/Contents/Resources/"
cp "$ROOT_DIR/resources/${APP_NAME}.icns" "$STAGE_APP/Contents/Resources/${APP_NAME}.icns"

echo "==> Signing ${APP_NAME}.app with Entitlements.plist..."
codesign --force --deep --sign - --entitlements "$ROOT_DIR/macos/Entitlements.plist" "$STAGE_APP"

echo "==> Stopping any running ${APP_NAME}..."
pkill -x "${APP_NAME}" 2>/dev/null || true
sleep 0.3

if [[ -d "$INSTALL_PATH" ]]; then
    rm -rf "$INSTALL_PATH"
fi
mv "$STAGE_APP" "$INSTALL_PATH"

if [[ "$RESET_PERMISSIONS" == "true" ]]; then
    echo "==> Resetting macOS TCC permissions for ${BUNDLE_ID}..."
    tccutil reset All "$BUNDLE_ID"
fi

echo "==> Installed to ${INSTALL_PATH} ($(du -sh "$INSTALL_PATH" | awk '{print $1}'))"
echo "==> Launching ${APP_NAME}..."
open -a "$INSTALL_PATH"
