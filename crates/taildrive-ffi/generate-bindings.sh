#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

cargo build --release -p taildrive-ffi
cargo build -p taildrive-ffi --features bindgen --bin uniffi-bindgen

case "$(uname -s)" in
    Darwin) library=target/release/libtaildrive_ffi.dylib ;;
    Linux) library=target/release/libtaildrive_ffi.so ;;
    *) echo "Build the bindings on macOS or Linux." >&2; exit 1 ;;
esac

swift_output=clients/macos/Sources/TailDriveFFI
android_output=clients/android/app/src/main/java
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT
target/debug/uniffi-bindgen generate "$library" --language swift --language kotlin \
    --crate taildrive_ffi --out-dir "$temp_dir" --no-format
mkdir -p "$swift_output/CTailDriveFFI/include" "$android_output/com/taildrive/ffi"

copy_binding() {
    awk '{ sub(/[ \t]+$/, ""); print }' "$temp_dir/$1" > "$2"
}

copy_binding TailDriveFFI.swift "$swift_output/TailDriveFFI.swift"
copy_binding CTailDriveFFI.h "$swift_output/CTailDriveFFI/include/CTailDriveFFI.h"
copy_binding CTailDriveFFI.modulemap "$swift_output/CTailDriveFFI/include/module.modulemap"
# UniFFI 0.32.2 emits two message properties when an error field is named
# message. Use the field itself as Throwable.message without changing the ABI.
awk '
    /^sealed class FfiException:/ { ffi_error = 1 }
    /^public object FfiConverterTypeFfiError/ { ffi_error = 0 }
    ffi_error && /val `message`: kotlin.String/ {
        sub(/val `message`/, "override val `message`")
        has_message = 1
    }
    ffi_error && has_message && /override val message$/ {
        getline
        has_message = 0
        next
    }
    { sub(/[ \t]+$/, ""); print }
' "$temp_dir/com/taildrive/ffi/taildrive_ffi.kt" > "$android_output/com/taildrive/ffi/taildrive_ffi.kt"
