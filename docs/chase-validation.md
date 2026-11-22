# 羲和号 RSM 数据验证

数据来自南京大学公开的论文数据目录。样本为羲和号 Hα 成像光谱仪（CHASE/HIS）的 RSM 一级光谱立方体。以下两个文件各约 10.1 GB。 [论文数据目录](https://sdc.nju.edu.cn/d/90242dc5043d46b1b140/) · [观测查询](https://ssdc.nju.edu.cn/NdchaseSatellite) · [定标说明](https://arxiv.org/html/2205.06075)

| UTC 观测时间 | 文件 | FITS 波长平面索引 | 参考平面索引 |
|---|---|---:|---:|
| 2021-12-05 18:26:54 | [RSM2021_12_05T18_26_54_HA.fits](https://sdc.nju.edu.cn/d/90242dc5043d46b1b140/files/?p=%2FRSM2021_12_05T18_26_54_HA.fits&dl=1) | 120–140 | 136 |
| 2021-12-07 17:55:16 | [RSM2021_12_07T17_55_16_HA.fits](https://sdc.nju.edu.cn/d/90242dc5043d46b1b140/files/?p=%2FRSM2021_12_07T17_55_16_HA.fits&dl=1) | 124–136 | 135 |

测试下载了上述索引对应的波长平面。转换程序沿狭缝和扫描方向各每隔一个像素采样，将 4608 × 4625 空间像素转换为 2304 × 2313，保留入选的 16 位像素值。参考平面取太阳盘内 80% 半径区域平均强度最低的实测通道。一级数据已完成羲和号的仪器定标，这项测试覆盖谱线选择、重建取样、太阳盘几何和输出读取。

## 用完整 FITS 重现

安装 Python、NumPy 和 Astropy。将 FITS 文件放入项目的 `target/chase`，再执行以下命令。原始 FITS、派生 SER 和图像输出均保存在 Git 忽略的目录。

```powershell
python -m pip install numpy astropy
python tests/chase_fixture.py target/chase/RSM2021_12_05T18_26_54_HA.fits target/chase/chase-20211205.ser --first 120 --last 140 --reference target/chase/chase-20211205-reference.fits
python tests/chase_fixture.py target/chase/RSM2021_12_07T17_55_16_HA.fits target/chase/chase-20211207.ser --first 124 --last 136 --reference target/chase/chase-20211207-reference.fits
cargo run --release --locked --example reconstruct -- target/chase/chase-20211205.ser target/chase/20211205-auto.fits
cargo run --release --locked --example reconstruct -- target/chase/chase-20211207.ser target/chase/20211207-fixed.fits --trace-row=11
```

索引从零开始。派生 SER 的每帧由一级数据重新排列，横轴为狭缝位置，纵轴为波长。应用的平均光谱由扫描中均匀选取的帧计算。2021-12-05 样本的色散是 +0.0242 Å/px。两组样本的红侧分别只保留 4 个和 1 个采样位置。第二组的参考平面位于裁切后 SER 的第 11 行，重建验证使用指定行。App 中可点击平均光谱相应的暗线。FITS 导出将线性强度保存在第一个 HDU，普通 PNG 使用预览的显示拉伸。

## 验证结果

用 Astropy 读取导出的 2304 × 2313 无符号 16 位 FITS。在参考盘面中心 750 像素半径内，第一组自动拟合结果与参考平面的 Pearson 相关系数为 **0.99849**，平均绝对强度差为 **0.703 DN**。第二组指定行的相关系数为 **0.99965**，平均绝对强度差为 **0.356 DN**。边缘几何拟合对两组都返回有效结果。

Android 14 x86_64 模拟器用第一组完成自动拟合、几何校正和 FITS 保存，用第二组完成手动选线和 FITS 保存。两个 16 位 FITS 分别与对应的桌面结果逐字节一致。Astropy 验证通过，模拟器日志中没有应用崩溃记录。ARM64 已通过构建和 Clippy 检查，实机性能待测。

## 合成平场验证

取 2021-12-05 的已定标样本为对照。令 `t = (x - 1292.21) / 935.17`，将 SER 中每帧的各波长行乘以列响应 `exp(0.10 × t − 0.11 × t²)`，四舍五入到 16 位样本。使用固定谱线行 `--trace-row=16` 重建。在太阳盘半径的 78% 内，计算强度与对照图之比，再除以比值的平均值。下表记录相对强度的 RMS 误差。

| 处理 | RMS 误差 |
|---|---:|
| 注入渐晕后直接重建 | 4.15% |
| 按 JSol’Ex 合成平场源码复现的参考算法 | 2.40% |
| MJSol'Ex 的合成平场 | 0.68% |

在原始已定标样本上，新算法跳过平场，输出像素逐一保持原值。对 2021-12-07 的样本，根据其 FITS 头确定盘心和半径，将注入响应的一次项反向，使用 `--trace-row=11` 重建。相对强度的 RMS 误差从 4.15% 降到 0.89%。

Windows release 构建对 2304 × 2313 像素样本执行盘缘定位、模型拟合和原地校正，预热后 7 次的中位数约为 23 ms。

## 显示导出与实拍平场流程验证

Android 14 x86_64 模拟器重建 2021-12-05 样本。16 位 PNG 的 5,329,152 个像素与桌面重建逐一一致，强度范围为 0–599 DN。普通 PNG 使用 0–500 DN 的显示拉伸，输出范围为 0–255，所有像素与独立计算一致。用正色散和 ±2 px 采样重建的双翼多普勒 PNG 与两翼桌面重建计算出的 RGB 像素逐一一致。

用上节注入渐晕的科学 SER 验证实拍平场入口。独立校准 SER 包含 4 帧模拟平场，逐列乘以相同的已知响应。模拟器中分别通过虚拟平场和实拍平场重建。实拍平场输出生成 Hα 假彩色 PNG，FITS 记录 `FLATCORR = 'PHYSICAL'` 和校准文件名。在盘心 730 像素半径内，实拍平场将归一化相对强度的 RMS 误差从 4.155% 降至 0.113%。这项测试验证了数据流程和已知渐晕的校正效果，实体相机采集的平场仍待验证。

## 性能复现

```powershell
cargo run --release --locked --example bench_scan -- target/chase/chase-20211205.ser 7 16
cargo run --release --locked --example bench_scan -- target/chase/chase-20211207.ser 7 11
```

Windows 使用同一份 SER、同一条指定采样行和 release 构建。首次运行预热，取随后 7 次的中位数。基线为提交 `46fbd8a`。读取量是 Rust 从文件读取的字节数，包含 SER 文件头。时间仅统计相应的计算阶段。

| 观测日期 | 平均光谱 | 重建 | 重建读取量 | 输出像素校验值 |
|---|---:|---:|---:|---|
| 2021-12-05 | 4.30 → 1.88 ms | 91.21 → 64.11 ms | 223,824,562 → 42,633,394 B | `2fbfbe06f9c11b56` |
| 2021-12-07 | 2.75 → 1.19 ms | 71.05 → 55.67 ms | 138,558,130 → 31,975,090 B | `2c8e92a21ebc6a46` |

Android 模拟器在共享存储中读取同一份 SER。按新版、旧版、旧版、新版顺序各测一轮。每轮预热一次，再取 7 次的中位数。表中范围为各版本两轮中位数的范围，输出校验值与 Windows 相同。

| 观测日期 | 平均光谱 | 重建 |
|---|---:|---:|
| 2021-12-05 | 9.76–10.45 → 2.39–2.52 ms | 121.95–137.61 → 87.28–100.05 ms |
| 2021-12-07 | 5.92–6.08 → 1.47–1.55 ms | 93.12–97.48 → 79.93–82.52 ms |
