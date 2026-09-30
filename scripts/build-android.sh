#!/usr/bin/env bash
# Build and package the Blockfall Android APK (debug-signed).
#
#   ANDROID_HOME=/path/to/android-sdk scripts/build-android.sh
#
# Steps: cargo-ndk cross-build of the `blockfall_app` cdylib (arm64-v8a),
# then raw aapt2/zipalign/apksigner packaging — no Gradle. The APK ships
# the native lib under lib/<abi>/ for the NativeActivity declared in
# android/AndroidManifest.xml and the app's `assets/` dir (bevy's Android
# asset reader opens paths directly against the APK asset root).
#
# Install on a device: $ANDROID_HOME/platform-tools/adb install -r target/android/blockfall-debug.apk
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

: "${ANDROID_HOME:?Set ANDROID_HOME to your Android SDK root}"
ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}
BUILD_TOOLS=${BUILD_TOOLS:-$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)}
PLATFORM=${PLATFORM:-$(basename "$(ls -d "$ANDROID_HOME"/platforms/android-* | sort -V | tail -1)")}
export ANDROID_NDK_HOME

ABIS=(arm64-v8a x86_64)
MIN_SDK=26
STAGE=$ROOT/target/android
mkdir -p "$STAGE"

echo "==> javac + d8 (immersive activity -> classes.dex)"
ANDROID_JAR="$ANDROID_HOME/platforms/$PLATFORM/android.jar"
DEX_OUT=$STAGE/dex
rm -rf "$DEX_OUT"
mkdir -p "$DEX_OUT/classes"
find "$ROOT/android/java" -name '*.java' > "$DEX_OUT/sources.txt"
javac -nowarn -source 8 -target 8 -bootclasspath "$ANDROID_JAR" \
    -d "$DEX_OUT/classes" @"$DEX_OUT/sources.txt"
"$BUILD_TOOLS/d8" --release --lib "$ANDROID_JAR" --output "$DEX_OUT" \
    $(find "$DEX_OUT/classes" -name '*.class')

echo "==> cargo-ndk build (release, ${ABIS[*]})"
NDK_ABIS=()
for abi in "${ABIS[@]}"; do NDK_ABIS+=(-t "$abi"); done
cargo ndk -o crates/tetris-app/android/jniLibs "${NDK_ABIS[@]}" --platform "$MIN_SDK" \
    build --release -p tetris-app --lib

echo "==> strip"
LLVM=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
for abi in "${ABIS[@]}"; do
    "$LLVM/llvm-strip" --strip-unneeded "crates/tetris-app/android/jniLibs/$abi/libblockfall_app.so"
done

echo "==> aapt2 link ($PLATFORM, build-tools $(basename "$BUILD_TOOLS"))"
rm -f "$STAGE"/*.apk
"$BUILD_TOOLS/aapt2" link \
    -o "$STAGE/blockfall-unsigned.apk" \
    -I "$ANDROID_HOME/platforms/$PLATFORM/android.jar" \
    --manifest android/AndroidManifest.xml \
    -A crates/tetris-app/assets \
    --min-sdk-version "$MIN_SDK" --target-sdk-version "${PLATFORM#android-}" \
    --version-code 3 --version-name 0.2.0

echo "==> adding native libs (stored, for extractNativeLibs=false)"
rm -rf "$STAGE/lib"
for abi in "${ABIS[@]}"; do
    mkdir -p "$STAGE/lib/$abi"
    cp "crates/tetris-app/android/jniLibs/$abi/libblockfall_app.so" "$STAGE/lib/$abi/"
done
cp "$DEX_OUT/classes.dex" "$STAGE/classes.dex"
(cd "$STAGE" && zip -q -X -0 blockfall-unsigned.apk classes.dex $(for abi in "${ABIS[@]}"; do echo "lib/$abi/libblockfall_app.so"; done))

echo "==> zipalign"
"$BUILD_TOOLS/zipalign" -f -p 4 "$STAGE/blockfall-unsigned.apk" "$STAGE/blockfall-aligned.apk"

echo "==> apksigner (debug key)"
KS=$ROOT/android/debug.keystore
if [[ ! -f $KS ]]; then
    keytool -genkeypair -keystore "$KS" -alias blockfall -keyalg RSA -keysize 2048 \
        -validity 10000 -storepass android -keypass android \
        -dname "CN=Android Debug,O=Android,C=US" 2>/dev/null
fi
"$BUILD_TOOLS/apksigner" sign \
    --ks "$KS" --ks-pass pass:android --key-pass pass:android \
    --out "$STAGE/blockfall-debug.apk" "$STAGE/blockfall-aligned.apk"

"$BUILD_TOOLS/apksigner" verify --print-certs "$STAGE/blockfall-debug.apk" | head -3
echo "APK: $STAGE/blockfall-debug.apk"
