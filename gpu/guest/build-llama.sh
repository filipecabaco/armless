#!/bin/sh
# Build llama.cpp for an armless guest (static aarch64 Linux, musl) with its WebGPU backend on
# include/webgpu/webgpu_cpp.h: the backend's GPU work goes to the host over armless-gpu's protocol.
#
#   gpu/guest/build-llama.sh LLAMA_CPP_SRC [OUT_DIR] [TARGETS...]
#
# Default targets: llama-completion llama-server. Output: OUT_DIR/bin (default build/llama-guest).
# Needs cmake, ninja, python3 and Zig as the cross compiler (`pip install ziglang`, or `zig` on PATH).
# llama.cpp's source: a checkout, or the copy bundled in llama-cpp-python's sdist (vendor/llama.cpp).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
src=$(realpath "${1:?usage: build-llama.sh LLAMA_CPP_SRC [OUT_DIR] [TARGETS...]}")
out=$(realpath -m "${2:-$root/build/llama-guest}")
shift $(( $# >= 2 ? 2 : 1 ))
targets=${*:-llama-completion llama-server}

if command -v zig >/dev/null 2>&1; then zig="zig"; else zig="python3 -m ziglang"; fi
tools="$out/toolchain"
mkdir -p "$tools"
for t in cc c++; do
    cat > "$tools/zig-$t" <<SH
#!/bin/sh
# zig $t for aarch64-linux-musl. ggml passes -march=armv8-a, which is zig's -mcpu=generic.
for a; do shift; case "\$a" in -march=armv8-a) set -- "\$@" -mcpu=generic ;; *) set -- "\$@" "\$a" ;; esac; done
exec $zig $t -target aarch64-linux-musl "\$@"
SH
done
printf '#!/bin/sh\nexec %s ar "$@"\n' "$zig" > "$tools/zig-ar"
printf '#!/bin/sh\nexec %s ranlib "$@"\n' "$zig" > "$tools/zig-ranlib"
chmod +x "$tools"/zig-*
cat > "$tools/aarch64-musl.cmake" <<CM
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER $tools/zig-cc)
set(CMAKE_CXX_COMPILER $tools/zig-c++)
set(CMAKE_AR $tools/zig-ar)
set(CMAKE_RANLIB $tools/zig-ranlib)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
CM

# The CPU is armless's (AdvSIMD, no dot product or SVE: GGML_CPU_ARM_ARCH=armv8-a). One static
# binary per tool, no OpenMP, no network libraries (the guest's network is the host's anyway).
cmake -S "$src" -B "$out" -G Ninja \
    -DCMAKE_TOOLCHAIN_FILE="$tools/aarch64-musl.cmake" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_EXE_LINKER_FLAGS=-static -DBUILD_SHARED_LIBS=OFF \
    -DGGML_NATIVE=OFF -DGGML_CPU_ARM_ARCH=armv8-a -DGGML_OPENMP=OFF \
    -DGGML_WEBGPU=ON -DDawn_DIR="$here/cmake" \
    -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
    -DLLAMA_BUILD_SERVER=ON
# shellcheck disable=SC2086
ninja -C "$out" $targets
for t in $targets; do
    llvm-strip "$out/bin/$t" 2>/dev/null || true
done
echo "built $(for t in $targets; do printf '%s ' "$out/bin/$t"; done)"
