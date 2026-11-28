#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

channel=${TAILDRIVE_CHANNEL:-dev}
build_number=${TAILDRIVE_BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}
commit_sha=${TAILDRIVE_COMMIT_SHA:-$(git rev-parse HEAD 2>/dev/null || echo dev)}
short_sha=$(printf '%s' "$commit_sha" | cut -c1-7)
built_at=${TAILDRIVE_BUILT_AT:-$(date -u +"%Y-%m-%dT%H:%M:%SZ")}
base_version=${TAILDRIVE_BASE_VERSION:-0.1.0}
arch=$(uname -m)
out_dir=${TAILDRIVE_DIST_DIR:-"$repo_root/dist"}

export TAILDRIVE_CHANNEL="$channel"
export TAILDRIVE_BUILD_NUMBER="$build_number"
export TAILDRIVE_COMMIT_SHA="$commit_sha"
export TAILDRIVE_BUILT_AT="$built_at"

cargo build --locked --release -p taildrive-ffi
bin_path=$(cd clients/macos && swift build -c release --show-bin-path)
# Force SwiftPM to relink against the freshly built libtaildrive_ffi.a
# (SwiftPM does not track -Xlinker static library mtimes across builds).
rm -f "$bin_path/TailDriveApp"
(cd clients/macos && swift build -c release --product TailDriveApp)
app_bundle="$out_dir/TailDrive.app"
rm -rf "$app_bundle"
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources"
cp "$bin_path/TailDriveApp" "$app_bundle/Contents/MacOS/TailDriveApp"
chmod 755 "$app_bundle/Contents/MacOS/TailDriveApp"

cat > "$app_bundle/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>TailDriveApp</string>
	<key>CFBundleIdentifier</key>
	<string>com.taildrive.macos</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>TailDrive</string>
	<key>CFBundleDisplayName</key>
	<string>TailDrive</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${base_version}</string>
	<key>CFBundleVersion</key>
	<string>${build_number}</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>TailDriveChannel</key>
	<string>${channel}</string>
	<key>TailDriveCommitSHA</key>
	<string>${commit_sha}</string>
	<key>TailDriveBuiltAt</key>
	<string>${built_at}</string>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$app_bundle"
codesign --verify --deep --strict "$app_bundle"

zip_path="$out_dir/TailDrive-macos-${arch}-${short_sha}.zip"
rm -f "$zip_path"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app_bundle" "$zip_path"
echo "Packaged $app_bundle"
echo "Archive $zip_path"
