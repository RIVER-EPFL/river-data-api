//! Inline keyboards and the callback payloads behind their buttons.
//!
//! Telegram caps `callback_data` at 64 bytes, which two hyphenated UUIDs overflow, so ids travel
//! base64url-encoded: 22 characters each, exact rather than a prefix. Everything arriving back is
//! untrusted, and an id that does not decode to 16 bytes is rejected before it reaches a query.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

/// One tappable button.
#[derive(Debug, Clone)]
pub struct Button {
    pub text: String,
    pub data: String,
}

/// Rows of buttons, rendered under a message.
pub type Keyboard = Vec<Vec<Button>>;

/// Telegram's `reply_markup` shape.
#[must_use]
pub fn markup(keyboard: &Keyboard) -> serde_json::Value {
    let rows: Vec<Vec<serde_json::Value>> = keyboard
        .iter()
        .map(|row| {
            row.iter()
                .map(|b| serde_json::json!({ "text": b.text, "callback_data": b.data }))
                .collect()
        })
        .collect();
    serde_json::json!({ "inline_keyboard": rows })
}

/// A UUID as it travels in a button payload.
#[must_use]
pub fn short(id: Uuid) -> String {
    URL_SAFE_NO_PAD.encode(id.as_bytes())
}

/// The inverse of [`short`]. `None` for anything that is not a 16-byte payload.
#[must_use]
pub fn from_short(s: &str) -> Option<Uuid> {
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    Uuid::from_slice(&bytes).ok()
}

fn valid_short(s: &str) -> bool {
    from_short(s).is_some()
}

/// What tapping a button asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The site picker.
    Sites,
    /// The parameter picker for one site.
    Parameters(String),
    /// Every parameter of one site, as a grid of panels.
    Overview(String),
    /// One series.
    View {
        site: String,
        parameter: String,
        window: String,
    },
}

impl Action {
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Action::Sites => "h".to_string(),
            Action::Parameters(site) => format!("s|{site}"),
            Action::Overview(site) => format!("g|{site}"),
            Action::View {
                site,
                parameter,
                window,
            } => format!("v|{site}|{parameter}|{window}"),
        }
    }

    /// Parse a button payload. `None` for anything malformed, which the caller reports as an
    /// expired button rather than trusting.
    #[must_use]
    pub fn parse(data: &str) -> Option<Action> {
        let parts: Vec<&str> = data.split('|').collect();
        match parts.as_slice() {
            ["h"] => Some(Action::Sites),
            ["s", site] if valid_short(site) => Some(Action::Parameters((*site).to_string())),
            ["g", site] if valid_short(site) => Some(Action::Overview((*site).to_string())),
            ["v", site, parameter, window]
                if valid_short(site)
                    && valid_short(parameter)
                    && super::plot_args::parse_window(window).is_some() =>
            {
                Some(Action::View {
                    site: (*site).to_string(),
                    parameter: (*parameter).to_string(),
                    window: (*window).to_string(),
                })
            }
            _ => None,
        }
    }
}

/// Windows offered under every chart.
pub const WINDOW_CHOICES: [&str; 4] = ["6h", "24h", "7d", "30d"];

/// The window switcher drawn under a chart, with the current window marked.
#[must_use]
pub fn window_row(site: &str, parameter: &str, current: &str) -> Vec<Button> {
    WINDOW_CHOICES
        .iter()
        .map(|w| Button {
            text: if *w == current {
                format!("• {w}")
            } else {
                (*w).to_string()
            },
            data: Action::View {
                site: site.to_string(),
                parameter: parameter.to_string(),
                window: (*w).to_string(),
            }
            .encode(),
        })
        .collect()
}

/// Lay buttons out `per_row` wide.
#[must_use]
pub fn rows(buttons: Vec<Button>, per_row: usize) -> Keyboard {
    buttons
        .chunks(per_row.max(1))
        .map(<[Button]>::to_vec)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_round_trips_a_uuid() {
        let id = Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000001").unwrap();
        assert_eq!(short(id).len(), 22);
        assert_eq!(from_short(&short(id)), Some(id));
        // Ids that share a prefix must stay distinct: the fixtures and any structured id scheme
        // differ only in their last group.
        let other = Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000002").unwrap();
        assert_ne!(short(id), short(other));
    }

    #[test]
    fn test_action_round_trips() {
        let id = Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000001").unwrap();
        let site = short(id);
        for action in [
            Action::Sites,
            Action::Parameters(site.clone()),
            Action::Overview(site.clone()),
            Action::View {
                site: site.clone(),
                parameter: short(Uuid::parse_str("aabbccdd-0000-4000-8000-000000000009").unwrap()),
                window: "6h".to_string(),
            },
        ] {
            assert_eq!(Action::parse(&action.encode()), Some(action));
        }
    }

    #[test]
    fn test_action_payload_fits_telegram_limit() {
        let action = Action::View {
            site: short(Uuid::new_v4()),
            parameter: short(Uuid::new_v4()),
            window: "30d".to_string(),
        };
        assert!(
            action.encode().len() <= 64,
            "payload was {} bytes",
            action.encode().len()
        );
    }

    #[test]
    fn test_action_rejects_malformed_payloads() {
        // Junk characters, a truncated id, an unknown verb, and a window that isn't one.
        let site = short(Uuid::new_v4());
        let parameter = short(Uuid::new_v4());
        for bad in [
            "s|%%%%%%%%".to_string(),
            "s|1a2b".to_string(),
            format!("x|{site}"),
            format!("v|{site}|{parameter}|9y"),
            format!("v|{site}|{parameter}"),
            String::new(),
            "h|extra".to_string(),
        ] {
            assert_eq!(Action::parse(&bad), None, "{bad} must not parse");
        }
    }

    #[test]
    fn test_window_row_marks_the_current_window() {
        let row = window_row(&short(Uuid::new_v4()), &short(Uuid::new_v4()), "7d");
        assert_eq!(row.len(), 4);
        assert_eq!(row[2].text, "• 7d");
        assert_eq!(row[0].text, "6h");
    }

    #[test]
    fn test_rows_chunks_buttons() {
        let buttons: Vec<Button> = (0..5)
            .map(|i| Button {
                text: i.to_string(),
                data: "h".to_string(),
            })
            .collect();
        let kb = rows(buttons, 2);
        assert_eq!(kb.len(), 3);
        assert_eq!(kb[2].len(), 1);
    }
}
