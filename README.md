# MJSol'Ex

基于 Astro4j JSol’Ex 的 Android 太阳扫描重建应用。名称中的 M 代表 Mobile。Rust 负责 SER 解码、谱线拟合、图像重建和 egui 界面渲染。Android NativeActivity 通过系统文件选择器打开和保存文件，处理过程在设备端完成。

## 使用

右上角可在简体中文和 English 之间切换。首次启动使用系统语言，之后记住上次选择。Android 系统文件选择器使用系统语言。

1. 在 **导入**（Import）点击 **打开 SER 文件** 选择太阳扫描。也可在靛空台素材库选中一个 SER，分享原文件到 **MJSol'Ex**。**打开靛空台**可以直接跳转。
2. 在 **Spectrum** 点击想用的暗线，或使用 **Auto-fit**。橙色曲线为谱线，调整取样偏移后青色曲线为实际取样位置。
3. **谱线剖面**（Line profile）显示对齐后的平均谱线、伪 Voigt 拟合和观测半高全宽。展开 **波长标定**，为图中已选的暗线选择 Hα、Ca II K、Ca II H、Hβ 或 He I D3，也可自定义零偏移波长。色散可以手动填写，或根据光栅刻线密度、衍射级次、两光臂夹角、成像镜焦距、相机像元尺寸及波长方向的像元合并倍数估算。使用 **填入标准 Sol'Ex 光路** 后，仍需填写自身相机的像元尺寸并指定波长方向。已定标的数据可手动输入数据头提供的色散。改选暗线后须重新指定谱线。
   - 标定完成后，界面叠加 BASS2000 参考曲线，并以 Å 显示观测半高全宽。谱翼覆盖充分时，两条曲线用同一波长窗口各自估计局部基线，线深为 `1 − 线芯强度 / 基线强度`。同时显示实测线深占参考线深的百分比。
4. 在 **处理选项 → 附加图像** 勾选一种或多种假彩色，也可勾选双翼多普勒。灰度结果始终生成。启用 **增强图（局部对比度）** 会另行生成增强灰度图及所选假彩色的增强版。大文件会自动增加帧间距，页面显示取样间隔和预期尺寸。
5. 点击 **重建图像** 后，在 **结果** 页切换各图。**保存全部** 将所有显示 PNG、线性 16 位 PNG 和 FITS 写入同一个选定文件夹。**保存当前 PNG** 单独导出当前图。线性文件保存采样中心的重建数据及已应用的平场和几何校正。

支持 1–16 位单色 SER、常见 Bayer 阵列、RGB/BGR 和 `JSOLEX-TRIMMED`。Bayer 取绿色通道，沿狭缝方向补全缺失像素。RGB/BGR 转成灰度。每帧上限为 8,388,608 像素，重建结果上限为 33,554,432 像素。平均光谱最多使用 64 帧，并按单帧大小限制读取量。自动字节序沿用靛空台的已知约定。其他 9–16 位 SER 先检查少量像素，证据不足时采用 SER Player 和 Siril 的常用约定（0 为小端，1 为大端）。若光谱异常，可在 **SER 详情**（SER details）手动选择字节序。

FITS 可记录原始 SER 文件名、UTC 时间、仪器、滤镜、像素偏移、帧间距、拟合宽度及几何校正参数，按已填写的色散和波长写入观测波长。天球坐标 WCS 和仪器通带定标尚需额外标定数据。 [JSol’Ex 功能覆盖](docs/jsolex-coverage.md) · [羲和号观测验证与下载](docs/chase-validation.md)

平场默认关闭。**虚拟平场**从完整太阳盘分别估计临边昏暗和沿狭缝方向的平滑亮度响应，保留天空背景基线。最大衰减为 20%，响应差低于 4% 时跳过。盘缘拟合失败或可用样本不足时，应用显示跳过原因。FITS 写入 `FLATCORR = 'SYNTHETIC'`。桌面示例可用 `--synthetic-flat` 运行同一算法。

**实拍平场**读取独立的 SER。使用与太阳扫描相同的相机 ROI、合并像素设置、颜色模式、位深和光路。平场应均匀照亮扫描中使用的整段狭缝，曝光应避开饱和。手动指定扫描的字节序时，平场沿用同一设置。如果自动模式发现扫描的字节序标志与像素相反，文件规格、标志及仪器信息匹配的平场在证据不足时沿用扫描的判定。应用估计各列的相对响应，在几何校正前应用一维增益。FITS 记录 `FLATCORR = 'PHYSICAL'` 和作为校准来源的 `FLATFILE`。重新导入扫描时需重新选取实拍平场。

双翼多普勒使用采样中心两侧的对称偏移。设定带正负号的色散后才能确定红蓝翼。两翼及其取样窗口都须落在已录制的波长范围内，条件不足时仍输出采样中心的图像。中心与双翼从每个取样帧一起提取，共用扫描间隔，单张图限制为 8 Mi 像素。红蓝显示相对谱翼强度差。实拍平场对三幅图共用校正系数，虚拟平场用于采样中心的强度图。

### 扫描校正和成图

处理顺序为谱线取样、平场、条带校正、装架周期抖动与盘缘锯齿校正、盘面几何、方向与裁切。增强图使用有对比度限制的自适应直方图均衡（CLAHE），与普通显示 PNG 一起导出。线性 PNG 和 FITS 保留增强前的强度。

- **扫描伪影校正**以旋转前的图像为准。沿扫描方向延伸的竖条选择 **竖条**，逐帧亮度变化造成的横条选择 **横条**。校正根据完整盘内的亮度和两侧盘缘估计。检测到稳定的周期摆动时校正装架抖动，盘缘锯齿校正处理周期模型之后的局部残差。
- **方向与裁切**先翻转，再顺时针旋转。自动裁切以盘心为中心，边长可选投影盘面直径的 1.1、1.2 或 1.5 倍。盘缘无法拟合时，应用保留可执行的方向设置。
- 谱线中心和两翼共用扫描校正、几何与构图。条带增益统一归一化以保留高光，FITS 的 `BANDSCL` 记录这个公共比例。FITS 还记录已应用的锯齿、抖动、旋转、翻转和裁切。抖动周期以校正前的重建行计，幅度以狭缝方向像素计。

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

在 Android 应用中打开 `demo-scan.ser`，使用默认偏移量重建并选择 **16 位 PNG**。输出应为 256 × 240 像素的灰度图，可与 `target/desktop-reference.png` 对比。测试包括 JSol’Ex `processSingleFrame` 的三个参考场景，覆盖高斯亚像素取样、图像边缘取样和已裁剪 SER 的取样。

已在 Android 14（API 34）x86_64 模拟器验证文件选择、无效文件提示、手动谱线拟合、偏移调整、重建和保存。导出的 61,440 个像素与桌面 Rust 测试程序逐一相同。ARM64 通过构建和 Clippy 检查，实机运行待验证。

## 来源与许可证

SER 格式和重建取样参考 Astro4j 的 `jserfile` 与 `jsolex-core`，基准提交为 `7ffdaf5c`。平均光谱从最多 64 个均匀选取的帧计算，移动版使用独立的二次曲线拟合。 [Astro4j 源码](https://github.com/melix/astro4j/tree/7ffdaf5c)

BASS2000 可见光参考数据采用 3000–9999.99 Å、0.01 Å 采样。可使用 `python tools/pack_bass2000.py <Astro4j>/jsolex-core/src/bass2000/atlasvi.dat assets/bass2000.bin` 重新生成。 [数据来源与许可证](assets/bass2000-NOTICE.txt)

Apache License 2.0。见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。
