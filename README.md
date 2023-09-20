# listenbrainz-poweramp
A PowerAmp plugin specifically for scrobbling your music to ListenBrainz

## Building
First, you need to build the NDK libraries
```sh
cd lbp_native
cargo install cargo-ndk
cargo ndk -p 30 -t armeabi-v7a -t arm64-v8a -o ../app/src/main/jniLibs/ build --release
```
Then you can build the APK as normal with Android Studio or gradle if you prefer.