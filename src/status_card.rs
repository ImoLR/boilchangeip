use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CountdownState {
    Duration(Duration),
    NotAvailable,
    Paused,
}

pub fn format_countdown(countdown: &CountdownState) -> String {
    match countdown {
        CountdownState::Duration(duration) => {
            let total = duration.as_secs();
            let days = total / 86_400;
            let hours = (total % 86_400) / 3_600;
            let minutes = (total % 3_600) / 60;
            let seconds = total % 60;
            if days > 0 {
                format!("{days}天 {hours:02}:{minutes:02}:{seconds:02}")
            } else {
                format!("{hours:02}:{minutes:02}:{seconds:02}")
            }
        }
        CountdownState::NotAvailable => "N/A".to_string(),
        CountdownState::Paused => "已暂停".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_formats_not_available() {
        assert_eq!(format_countdown(&CountdownState::NotAvailable), "N/A");
    }

    #[test]
    fn countdown_formats_more_than_one_day() {
        assert_eq!(
            format_countdown(&CountdownState::Duration(Duration::from_secs(
                2 * 86_400 + 3 * 3_600 + 18 * 60 + 42,
            ))),
            "2天 03:18:42"
        );
    }

    #[test]
    fn countdown_formats_paused() {
        assert_eq!(format_countdown(&CountdownState::Paused), "已暂停");
    }
}
