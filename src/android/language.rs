use std::sync::Arc;

use eframe::egui::{Context, FontData, FontDefinitions};

use super::{CorrectionWarnings, sequence};
use crate::Error;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Language {
    English,
    Chinese,
}

impl Language {
    pub(super) fn from_code(code: &str) -> Self {
        match code {
            "zh" => Self::Chinese,
            _ => Self::English,
        }
    }

    pub(super) fn code(self) -> &'static str {
        self.text("en", "zh")
    }

    pub(super) fn other(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }

    pub(super) fn text(self, english: &'static str, chinese: &'static str) -> &'static str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }

    pub(super) fn error(self, error: &Error) -> String {
        if self == Self::English {
            return error.to_string();
        }
        match error {
            Error::Io(error) => format!("文件读写失败：{error}"),
            Error::InvalidSer(reason) => {
                let reason = match *reason {
                    "incomplete header" => "文件头不完整",
                    "unrecognized recorder signature" => "无法识别录制格式",
                    "invalid byte order flag" => "字节序标记无效",
                    "empty frame or recording" => "帧或录制为空",
                    "frame dimensions exceed 16384 pixels" => "单帧的宽度或高度超过 16384 像素",
                    "a Bayer frame needs at least two columns" => "Bayer 图像帧至少需要两列",
                    "frame exceeds 8 Mi pixels" => "单帧像素数超过上限",
                    "invalid pixel depth" => "像素位深无效",
                    "frame data is truncated" => "帧数据不完整",
                    "frame index outside the recording" => "帧索引超出录制范围",
                    "recording changed; open the finished file again" => {
                        "录制文件已更改，请在录制完成后重新打开"
                    }
                    _ => reason,
                };
                format!("SER 文件无效：{reason}")
            }
            Error::Color(id) => format!("不支持色彩模式为 {id} 的 SER。"),
            Error::NoLine => "未找到可靠的吸收线。请在光谱图中点选一条暗线。".into(),
            Error::InvalidTrace => "谱线或采样偏移超出录制范围。".into(),
            Error::NoSolarLimb => "无法拟合完整的太阳边缘。".into(),
            Error::InvalidFraming => "旋转或裁切设置无效。".into(),
            Error::NoFlatSamples => "用于估计合成平场的盘内样本不足。".into(),
            Error::FlatMismatch => "平场 SER 的尺寸、颜色模式和位深必须与扫描文件一致。".into(),
            Error::SequenceMismatch => {
                "扫描的尺寸、颜色模式、位深和裁剪格式须与参考 SER 一致。".into()
            }
            Error::FlatCoverage => "扫描使用的狭缝列超出了实拍平场的校准范围。".into(),
            Error::InvalidPhysicalFlat => "平场 SER 未能充分照亮整条狭缝。".into(),
            Error::DopplerCoverage => "当前扫描无法覆盖所选偏移处的两侧谱翼。".into(),
            Error::DopplerDispersion => "请先设定波长色散和方向。".into(),
            Error::OutputTooLarge => "重建图像的像素数超过上限。".into(),
            Error::InvalidStride => "帧间距至少为 1。".into(),
            Error::Allocation(_) => "可用内存不足，无法处理此录制。".into(),
            Error::Cancelled => "处理已取消。".into(),
            Error::CacheLimit { required, limit } => {
                format!("此 SER 需要 {required} 字节临时存储，缓存上限为 {limit} 字节。")
            }
            Error::Png(error) => format!("PNG 导出失败：{error}"),
        }
    }

    pub(super) fn corrections(self, warnings: &CorrectionWarnings) -> String {
        warnings
            .entries()
            .into_iter()
            .filter_map(|(error, english, chinese)| {
                let prefix = self.text(english, chinese);
                error.as_ref().map(|e| format!("{prefix}{}", self.error(e)))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub(super) enum Notice {
    Processing(Error),
    Document(String),
    FilePicker(String),
    SharedRecording(String),
    IndigoUnavailable,
    IndigoLaunch(String),
    Corrections(Box<CorrectionWarnings>),
    Saved(String),
    BatchSaved { saved: usize, error: Option<Error> },
    Sequence(sequence::Summary),
    SaveLanguage(String),
}

impl Notice {
    pub(super) fn is_error(&self) -> bool {
        if let Self::Sequence(summary) = self {
            return !summary.issues.is_empty();
        }
        !matches!(
            self,
            Self::Saved(_)
                | Self::BatchSaved { error: None, .. }
                | Self::Processing(Error::Cancelled)
        )
    }

    pub(super) fn message(&self, language: Language) -> String {
        let with_detail = |english, chinese, detail: &str| {
            let title = language.text(english, chinese);
            match language {
                Language::English => format!("{title}: {detail}"),
                Language::Chinese => format!("{title}：{detail}"),
            }
        };
        match self {
            Self::Processing(error) => language.error(error),
            Self::Document(code) => match code.as_str() {
                "picker" => language
                    .text(
                        "Cannot open the Android file picker.",
                        "无法打开 Android 文件选择器。",
                    )
                    .into(),
                "single_ser" => language
                    .text(
                        "Share one original SER recording.",
                        "请分享一份原始 SER 录制。",
                    )
                    .into(),
                "unavailable" => language
                    .text(
                        "The file provider did not open the document.",
                        "文件提供方未能打开文件。",
                    )
                    .into(),
                "access" => language
                    .text("Cannot access the document.", "无法访问文件。")
                    .into(),
                detail => with_detail(
                    "Cannot access the document",
                    "无法访问文件",
                    detail.strip_prefix("access:").unwrap_or(detail),
                ),
            },
            Self::FilePicker(detail) => {
                with_detail("Cannot open file picker", "无法打开文件选择器", detail)
            }
            Self::SharedRecording(detail) => with_detail(
                "Cannot read the shared recording",
                "无法读取分享的录制",
                detail,
            ),
            Self::IndigoUnavailable => language
                .text(
                    "Indigo Observatory is unavailable on this phone.",
                    "此设备上的靛空台不可用。",
                )
                .into(),
            Self::IndigoLaunch(detail) => {
                with_detail("Cannot open Indigo Observatory", "无法打开靛空台", detail)
            }
            Self::Corrections(warnings) => language.corrections(warnings),
            Self::Saved(name) => {
                let prefix = language.text("Saved ", "已保存 ");
                format!("{prefix}{name}")
            }
            Self::BatchSaved { saved, error } => match (language, error) {
                (Language::English, None) => format!("Saved {saved} files."),
                (Language::Chinese, None) => format!("已保存 {saved} 个文件。"),
                (Language::English, Some(error)) => format!(
                    "Saved {saved} files. Export stopped: {}",
                    language.error(error)
                ),
                (Language::Chinese, Some(error)) => format!(
                    "已保存 {saved} 个文件。保存已停止。{}",
                    language.error(error)
                ),
            },
            Self::Sequence(summary) => {
                let sequence::Summary {
                    total,
                    completed,
                    failed,
                    saved,
                    cancelled,
                    ..
                } = summary;
                let status = if *cancelled {
                    language.text("Sequence cancelled.", "序列已取消。")
                } else {
                    language.text("Sequence finished.", "序列处理结束。")
                };
                match language {
                    Language::English => format!(
                        "{status} {completed}/{total} scans complete, {failed} failed, {saved} files saved."
                    ),
                    Language::Chinese => format!(
                        "{status} 已完成 {completed}/{total} 份扫描，失败 {failed} 份，已保存 {saved} 个文件。"
                    ),
                }
            }
            Self::SaveLanguage(detail) => with_detail(
                "Could not save language preference",
                "无法保存语言设置",
                detail,
            ),
        }
    }
}

pub(super) fn install_fonts(context: &Context) {
    const FONT: &str = "mjsolex-chinese";
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        FONT.to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/MJSolexSansSC.ttf"
        ))),
    );
    for family in fonts.families.values_mut() {
        family.push(FONT.to_owned());
    }
    context.set_fonts(fonts);
}
