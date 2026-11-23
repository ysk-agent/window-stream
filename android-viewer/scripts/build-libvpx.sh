#!/usr/bin/env bash
# Build libvpx (static, PIC) for the three Android ABIs the viewer
# ships, so the shared codec crate can link it via pkg-config.
#
# Why from source: the codec crate links the system libvpx through
# pkg-config and generates bindings from its headers (see
# crates/codec/build.rs); there is no system libvpx for Android, so CI
# builds one per ABI and points PKG_CONFIG_PATH_<target> at the
# per-ABI installs (see .github/workflows/android-viewer.yml).
#
# libvpx's configure assumes a "standalone build with NDK toolchain"
# for *-android-gcc targets: the compiler names come from the
# environment (CC/CXX/AR/AS/STRIP/RANLIB), which this script sets from
# the NDK's LLVM toolchain.
#
# Usage: build-libvpx.sh <ndk-home> <output-root> [libvpx-ref]
set -euo pipefail

NDK_HOME="$1"
OUT_ROOT="$2"
VPX_REF="${3:-v1.14.1}"
API=29

SRC=/tmp/libvpx-src
if [ ! -d "$SRC" ]; then
  git clone --depth 1 --branch "$VPX_REF" https://github.com/webmproject/libvpx.git "$SRC"
fi

TOOLCHAIN_BIN="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"

# args: <vpx-target> <clang-binary-prefix> <abi> <as-mode>
# as-mode "clang" uses the clang driver with -c (ARM assembly);
# as-mode "auto" leaves AS unset so configure finds yasm/nasm (x86
# assembly is NASM-format and configure adds -f elf64 itself).
build_one() {
  local vpx_target="$1" cc_name="$2" abi="$3" as_mode="$4"
  local prefix="$OUT_ROOT/$abi"
  local build_dir="/tmp/libvpx-build-$abi"
  rm -rf "$build_dir"
  mkdir -p "$build_dir"
  (
    cd "$build_dir"
    export CC="$TOOLCHAIN_BIN/$cc_name"
    export CXX="$TOOLCHAIN_BIN/${cc_name}++"
    # LD must be set explicitly: with CROSS unset, libvpx's configure
    # defaults LD to the host `ld`, which then rejects the target
    # objects ("file in wrong format"). The clang driver links fine.
    export LD="$TOOLCHAIN_BIN/$cc_name"
    export AR="$TOOLCHAIN_BIN/llvm-ar"
    # AS gets "-c": libvpx's armv7 .asm.S rule invokes $(AS) without
    # -c (a standalone `as` always emits an object), but the clang
    # driver would try to link an executable and fail on `main`.
    if [ "$as_mode" = "clang" ]; then
      export AS="$TOOLCHAIN_BIN/$cc_name -c"
    else
      unset AS
    fi
    export STRIP="$TOOLCHAIN_BIN/llvm-strip"
    export RANLIB="$TOOLCHAIN_BIN/llvm-ranlib"
    export NM="$TOOLCHAIN_BIN/llvm-nm"
    echo "== libvpx $abi: $CC"
    "$CC" --version | head -1
    "$SRC/configure" \
      --target="$vpx_target" \
      --prefix="$prefix" \
      --disable-examples \
      --disable-tools \
      --disable-docs \
      --disable-unit-tests \
      --disable-webm-io \
      --disable-libyuv \
      --enable-pic \
      --extra-cflags="-fPIC" || {
        echo "== configure failed; tail of config.log:"
        tail -60 config.log || true
        exit 1
      }
    make -j"$(nproc)"
    make install
  )
  echo "libvpx for $abi installed:"
  ls -l "$prefix/lib/libvpx.a" "$prefix/lib/pkgconfig/vpx.pc"
}

build_one arm64-android-gcc "aarch64-linux-android${API}-clang" arm64-v8a clang
build_one armv7-android-gcc "armv7a-linux-androideabi${API}-clang" armeabi-v7a clang
build_one x86_64-android-gcc "x86_64-linux-android${API}-clang" x86_64 auto
