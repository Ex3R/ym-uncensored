#!/bin/bash
# Собирает APK для arm64-v8a без Gradle, как android/build.ps1, но на Linux (для CI).
# Нужны ANDROID_HOME (SDK с NDK, build-tools и platforms) и JAVA_HOME.
# Подпись: ключ из ANDROID_KEYSTORE (base64), ANDROID_KEYSTORE_PASSWORD и ANDROID_KEY_ALIAS.
# Без них APK подписывается временным отладочным ключом: такой годится только для проверки.
# Результат: target/apk/yamusic-rs-arm64-v8a.apk.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${ANDROID_HOME:?ANDROID_HOME не задан}"
: "${JAVA_HOME:?JAVA_HOME не задан}"
latest() { ls -d "$1"/*/ | sort -V | tail -1 | sed 's:/$::'; }
NDK=$(latest "$ANDROID_HOME/ndk")
BT=$(latest "$ANDROID_HOME/build-tools")
# Как targetSdkVersion в манифесте: у раннера самая новая платформа бывает предварительной.
PLATFORM="$ANDROID_HOME/platforms/android-35"
[ -d "$PLATFORM" ] || PLATFORM=$(ls -d "$ANDROID_HOME"/platforms/android-*/ | sed 's:/$::' | sort -t- -k2 -n | tail -1)

ABI=arm64-v8a
TRIPLE=aarch64-linux-android
API=26
case "$(uname -s)" in
  Linux) HOST_TAG=linux-x86_64 ;;
  Darwin) HOST_TAG=darwin-x86_64 ;;
  *) echo "Неизвестная платформа сборки NDK" >&2; exit 1 ;;
esac
LLVM="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
export CC_aarch64_linux_android="$LLVM/bin/$TRIPLE$API-clang"
export CXX_aarch64_linux_android="$LLVM/bin/$TRIPLE$API-clang++"
export AR_aarch64_linux_android="$LLVM/bin/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
export ANDROID_NDK_ROOT="$NDK" ANDROID_NDK="$NDK" ANDROID_NDK_HOME="$NDK"
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android="--sysroot=$LLVM/sysroot ${BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android:-}"

TARGET="${CARGO_TARGET_DIR:-android/target}"
if [ "$(uname -s)" = Darwin ]; then
  # whisper-rs-sys 0.15 requests its macOS BLAS archive based on the build host.
  # Android actually builds ggml-cpu with BLAS off. No symbols from BLAS are used.
  BLAS_DIR="$PWD/android/target/android-cpu-only"
  mkdir -p "$BLAS_DIR"
  "$AR_aarch64_linux_android" rcs "$BLAS_DIR/libggml-blas.a"
  export GGML_BLAS=OFF
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="${CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS:-} -Lnative=$BLAS_DIR"
fi
if [ "${CI:-}" = true ] || [ "${YM_PUBLIC_BUILD:-}" = 1 ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/build -Lnative=$PWD/android/target/android-cpu-only"
  export CFLAGS_aarch64_linux_android="${CFLAGS_aarch64_linux_android:-} -ffile-prefix-map=$HOME=/build"
  export CXXFLAGS_aarch64_linux_android="${CXXFLAGS_aarch64_linux_android:-} -ffile-prefix-map=$HOME=/build"
fi
cargo build --release --locked --target "$TRIPLE" --manifest-path android/Cargo.toml --target-dir "$TARGET"

if [ "${CI:-}" = true ] || [ "${YM_PUBLIC_BUILD:-}" = 1 ]; then
  if LC_ALL=C grep -aFq "$HOME/" "$TARGET/$TRIPLE/release/libyamusic.so"; then
    echo "В libyamusic.so остался домашний путь сборщика" >&2
    exit 1
  fi
fi

OUT=target/apk
STAGE="$OUT/stage"
rm -rf "$OUT"
mkdir -p "$STAGE/lib/$ABI"
mkdir -p "$STAGE/assets/licenses"
cp models/*-LICENSE "$STAGE/assets/licenses/"
cp "$TARGET/$TRIPLE/release/libyamusic.so" "$STAGE/lib/$ABI/"
# oboe собран с c++_shared: библиотеку надо положить рядом.
cp "$LLVM/sysroot/usr/lib/$TRIPLE/libc++_shared.so" "$STAGE/lib/$ABI/"

UNSIGNED="$OUT/unsigned.apk"
"$BT/aapt2" compile --dir android/res -o "$OUT/res.zip"
"$BT/aapt2" link -o "$UNSIGNED" --manifest android/AndroidManifest.xml -I "$PLATFORM/android.jar" --java "$OUT/gen" "$OUT/res.zip"

# Java нужен только сервису шторки: у NativeActivity своего кода нет.
find android/java "$OUT/gen" -name '*.java' > "$OUT/sources.txt"
"$JAVA_HOME/bin/javac" --release 11 -nowarn -encoding UTF-8 -classpath "$PLATFORM/android.jar" -d "$OUT/classes" @"$OUT/sources.txt"
"$BT/d8" --release --min-api "$API" --lib "$PLATFORM/android.jar" --output "$STAGE" $(find "$OUT/classes" -name '*.class')

(cd "$STAGE" && zip -q -r ../unsigned.apk .)
"$BT/zipalign" -P 16 -f 4 "$UNSIGNED" "$OUT/aligned.apk"

APK="$OUT/yamusic-rs-$ABI.apk"
# Подпись v1 apksigner при minSdk 26 пропускает, а установщики некоторых оболочек без неё пишут «нет сертификата».
SIGN="--v1-signing-enabled true"
if [ -n "${ANDROID_KEYSTORE:-}" ]; then
  KS="$OUT/release.keystore"
  printf '%s' "$ANDROID_KEYSTORE" | base64 --decode > "$KS"
  "$BT/apksigner" sign $SIGN --ks "$KS" --ks-pass env:ANDROID_KEYSTORE_PASSWORD --ks-key-alias "$ANDROID_KEY_ALIAS" --out "$APK" "$OUT/aligned.apk"
  rm -f "$KS"
else
  if [ "${REQUIRE_RELEASE_SIGNATURE:-0}" = "1" ]; then
    echo "Для публикуемого APK нужен постоянный ключ подписи" >&2
    exit 1
  fi
  echo "::warning::Нет ключа выпуска: APK подписан временным ключом, в релиз он не годится"
  KS="$OUT/debug.keystore"
  "$JAVA_HOME/bin/keytool" -genkeypair -keystore "$KS" -storepass android -alias debug -keypass android \
    -dname 'CN=Debug' -keyalg RSA -keysize 2048 -validity 30 >/dev/null
  "$BT/apksigner" sign $SIGN --ks "$KS" --ks-pass pass:android --out "$APK" "$OUT/aligned.apk"
fi
"$BT/apksigner" verify "$APK"
ls -l "$APK"
