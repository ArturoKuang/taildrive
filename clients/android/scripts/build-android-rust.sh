#!/bin/sh
# Run with ANDROID_HOME set, using Rust 1.100+ and the Android NDK.
# Install the toolchain with sdkmanager 'ndk;28.2.13676358' 'cmake;3.22.1'
# and rustup target add aarch64-linux-android. ANDROID_NDK_HOME can override
# the SDK NDK location. Run Gradle assembleDebug after this script.
set -eu

android_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$android_dir/../.."

sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
ndk=${ANDROID_NDK_HOME:-${sdk:+$sdk/ndk/28.2.13676358}}
if [ -z "$ndk" ] || [ ! -d "$ndk/toolchains/llvm/prebuilt" ]; then
    echo "Set ANDROID_HOME or ANDROID_NDK_HOME and install Android NDK r28c (28.2.13676358)." >&2
    exit 1
fi

case "$(uname -s)-$(uname -m)" in
    Darwin-*) host=darwin-x86_64 ;;
    Linux-x86_64) host=linux-x86_64 ;;
    *) echo "Use a supported macOS or x86-64 Linux NDK host." >&2; exit 1 ;;
esac

toolchain="$ndk/toolchains/llvm/prebuilt/$host/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$toolchain/aarch64-linux-android26-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export CXX_aarch64_linux_android="$toolchain/aarch64-linux-android26-clang++"
export AR_aarch64_linux_android="$toolchain/llvm-ar"
export ANDROID_NDK_HOME="$ndk"
if [ -n "$sdk" ] && [ -d "$sdk/cmake/3.22.1/bin" ]; then
    export PATH="$sdk/cmake/3.22.1/bin:$PATH"
fi
if [ ! -x "$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER" ]; then
    echo "Android API 26 clang not found in $toolchain." >&2
    exit 1
fi

build_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked --release -p taildrive-ffi --target aarch64-linux-android --target-dir "$build_dir"
output="$android_dir/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$output"
install -m 644 "$build_dir/aarch64-linux-android/release/libtaildrive_ffi.so" "$output/"
echo "Built $output/libtaildrive_ffi.so"
