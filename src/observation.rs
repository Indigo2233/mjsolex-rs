use time::OffsetDateTime;

/// Values carried by the SER header and its optional timestamp trailer.
#[derive(Clone, Debug, Default)]
pub struct Observation {
    pub observer: Option<String>,
    pub instrument: Option<String>,
    pub telescope: Option<String>,
    pub filter: Option<String>,
    pub recorder: Option<&'static str>,
    pub start_utc: Option<String>,
    pub end_utc: Option<String>,
}

impl Observation {
    pub(crate) fn from_header(bytes: &[u8; 178]) -> Self {
        let source = |field: &[u8]| {
            let end = field
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(field.len());
            let text = String::from_utf8_lossy(&field[..end]);
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        let observed = source(&bytes[42..82]);
        let indigo = observed
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("MobileObservatory"));
        let telescope_or_filter = source(&bytes[122..162]);
        let (telescope, filter) = if indigo {
            (None, telescope_or_filter)
        } else {
            (telescope_or_filter, None)
        };
        let header_utc_ticks =
            u64::from_le_bytes(bytes[170..178].try_into().expect("SER UTC field"));
        Self {
            observer: if indigo { None } else { observed },
            instrument: source(&bytes[82..122]),
            telescope,
            filter,
            recorder: indigo.then_some("Indigo Observatory"),
            start_utc: format_ser_utc(header_utc_ticks),
            end_utc: None,
        }
    }

    pub(crate) fn set_trailer(&mut self, first: u64, last: u64) {
        if first <= last
            && let (Some(start), Some(end)) = (format_ser_utc(first), format_ser_utc(last))
        {
            // The trailer records the acquisition times of individual frames.
            self.start_utc = Some(start);
            self.end_utc = Some(end);
        }
    }
}

fn format_ser_utc(ticks: u64) -> Option<String> {
    if ticks == 0 {
        return None;
    }
    const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;
    let date =
        OffsetDateTime::from_unix_timestamp_nanos((i128::from(ticks) - UNIX_EPOCH_TICKS) * 100)
            .ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}",
        date.year(),
        date.month() as u8,
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
        date.nanosecond() / 100
    ))
}
