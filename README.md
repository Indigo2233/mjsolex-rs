# MJ Sol’Ex

基于 Astro4j JSol’Ex 的 Android 太阳扫描重建应用。Rust 负责 SER 解码、谱线拟合、图像重建和 egui 界面渲染。Android NativeActivity 通过系统文件选择器打开和保存文件，处理过程在设备端完成。

## 使用

1. 在 **Recording** 点击 **Open SER file** 选择太阳扫描。也可在靛空台素材库选中一个 SER，分享原文件到 **MJ Sol’Ex**。**Open Indigo Observatory** 可以直接打开本机靛空台。
2. 在 **Spectrum** 点击想用的暗线，或使用 **Auto-fit**。橙色曲线为谱线，调整取样偏移后青色曲线为实际取样位置。
3. **Line profile** 显示对齐后的平均谱线、伪 Voigt 拟合和观测半高全宽。输入色散后同时显示 Å 数值。可在 **Wavelength calibration** 录入零偏移对应的波长及带符号的色散。
4. 可选 **Correct full-disk geometry** 校正扫描轴比例和剪切。超过 32 Mi 像素的扫描默认增加帧间距，页面显示取样间隔和预期尺寸。
5. 点击 **Reconstruct scan**。在 **Result** 选择 **Save FITS** 或 **Save PNG**。两者保留 16 位灰度样本，预览对比度只用于显示。

支持 1–16 位单色 SER、常见 Bayer 阵列、RGB/BGR 和 `JSOLEX-TRIMMED`。Bayer 取绿色通道，沿狭缝方向补全缺失像素。RGB/BGR 转成灰度。每帧上限为 8,388,608 像素，重建结果上限为 33,554,432 像素。平均光谱最多使用 64 帧，并按单帧大小限制读取量。在 **Recording details** 可以手动切换 9–16 位 SER 的字节序。

FITS 可记录原始 SER 文件名、UTC 时间、仪器、滤镜、像素偏移、帧间距、拟合宽度及几何校正参数，按已填写的色散和波长写入观测波长。天球坐标 WCS 和仪器通带定标尚需额外标定数据。 [JSol’Ex 功能覆盖](docs/jsolex-coverage.md) · [羲和号观测验证与下载](docs/chase-validation.md)

若文件提供方只能以数据流交付，应用先验证 SER 文件头，再将帧数据暂存到本机。暂存上限为应用缓存所在分区的可用空间的一半。可随机读取的文件直接处理。

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
