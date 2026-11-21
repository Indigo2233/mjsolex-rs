# MJ Sol’Ex

基于 Astro4j JSol’Ex 的 Android 太阳扫描重建应用。Rust 负责 SER 解码、谱线拟合、图像重建和 egui 界面渲染。Android NativeActivity 通过系统文件选择器打开和保存文件，处理过程在设备端完成。

## 使用

1. 点击 **Open SER recording**，选择单色太阳扫描 SER 文件。
2. 查看平均光谱和橙色采样曲线。点击暗谱线调整拟合，或点击 **Auto-fit line** 恢复自动拟合。
3. 使用 **Spectral offset** 以 0.25 像素为步长调整沿光谱方向的采样位置，点击 **Reconstruct scan**。
4. 预览重建图像，点击 **Save 16-bit PNG**，选择保存位置。预览会拉伸对比度，PNG 保存 16 位灰度采样值。

当前版本输出原始扫描坐标中的单张灰度图。支持 1–16 位单色 SER 和 `JSOLEX-TRIMMED`，每帧上限为 8,388,608 像素，输出上限为 33,554,432 像素。JSol’Ex 桌面版的太阳盘几何校正、波长识别、FITS、彩色处理、脚本、堆栈和批处理尚未移植。

## 构建

在 Windows 安装 Rust 1.95 或更新版本、JDK 17 或更新版本、Android SDK Platform 34、Build Tools 34.0.0 和 Android NDK。手机目标为 Android 8.0（API 26）或更新版本，以及 OpenGL ES 3.0。

```powershell
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
./android/build-apk.ps1 -Jdk 'C:/Program Files/Java/jdk-21'
adb install -r target/apk/mjsolex-debug.apk
```

脚本从 `ANDROID_HOME` 或 `ANDROID_SDK_ROOT` 读取 SDK 路径，也可传入 `-Sdk`。设置 `JAVA_HOME` 后可省略 `-Jdk`。默认 APK 同时包含 ARM64 和 x86_64，使用本地调试签名。可通过 `-Abi arm64-v8a` 或 `-Abi x86_64` 单独构建。

## 测试

```powershell
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo ndk -t x86_64 -t arm64-v8a --platform 26 clippy --locked --lib -- -D warnings
python tests/make_demo_ser.py target/demo-scan.ser
cargo run --locked --example reconstruct -- target/demo-scan.ser target/desktop-reference.png
adb push target/demo-scan.ser /sdcard/Download/
```

在 Android 应用中打开 `demo-scan.ser`，使用默认偏移量重建并导出。输出应为 256 × 240 像素的 16 位灰度 PNG，可与 `target/desktop-reference.png` 对比。测试包括 JSol’Ex `processSingleFrame` 的三个参考场景，覆盖高斯亚像素取样、图像边缘取样和已裁剪 SER 的取样。

已在 Android 14（API 34）x86_64 模拟器验证文件选择、无效文件提示、手动谱线拟合、偏移调整、重建和保存。导出的 61,440 个像素与桌面 Rust 测试程序逐一相同。ARM64 通过构建和 Clippy 检查，实机运行待验证。

## 来源与许可证

SER 格式和重建取样参考 Astro4j 的 `jserfile` 与 `jsolex-core`，基准提交为 `7ffdaf5c`。平均光谱从最多 64 个均匀选取的帧计算，移动版使用独立的二次曲线拟合。 [Astro4j 源码](https://github.com/melix/astro4j/tree/7ffdaf5c)

Apache License 2.0。见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。
