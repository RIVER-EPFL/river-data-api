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
    /// Site picker for `/latest`, then that site's latest readings.
    LatestSites,
    Latest(String),
    /// Site picker for `/thresholds`, then that site's thresholds.
    ThresholdSites,
    Thresholds(String),
    /// Site picker for `/battery`, then that site's battery reading.
    BatterySites,
    Battery(String),
    /// The four steps of muting: which site, which parameter, then the durations, then the write.
    MuteSites,
    MuteParams(String),
    MuteWhen {
        site: String,
        parameter: String,
    },
    MuteSet {
        site: String,
        parameter: String,
        /// Days, or `0` for no expiry.
        days: i64,
    },
    /// The mute listing, which is also how a mute is lifted.
    Muted,
    UnmuteSet {
        site: String,
        parameter: String,
    },
}

impl Action {
    /// Whether tapping this changes data. Write actions are refused outside a 1:1 chat, matching
    /// the typed commands: in a group every member shares one chat id, so no individual owns the
    /// act. The mute pickers count, since their whole purpose is to reach a write.
    #[must_use]
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Action::MuteSites
                | Action::MuteParams(_)
                | Action::MuteWhen { .. }
                | Action::MuteSet { .. }
                | Action::UnmuteSet { .. }
        )
    }

    /// Whether tapping this needs an administrator, mirroring the gate on the typed command. The
    /// button may have been sent before the tapper's role changed, so this is re-checked per tap.
    #[must_use]
    pub fn requires_admin(&self) -> bool {
        self.is_write() || matches!(self, Action::Muted)
    }
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
            Action::LatestSites => "l".to_string(),
            Action::Latest(site) => format!("L|{site}"),
            Action::ThresholdSites => "t".to_string(),
            Action::Thresholds(site) => format!("T|{site}"),
            Action::BatterySites => "b".to_string(),
            Action::Battery(site) => format!("B|{site}"),
            Action::MuteSites => "m".to_string(),
            Action::MuteParams(site) => format!("n|{site}"),
            Action::MuteWhen { site, parameter } => format!("w|{site}|{parameter}"),
            Action::MuteSet {
                site,
                parameter,
                days,
            } => format!("k|{site}|{parameter}|{days}"),
            Action::Muted => "u".to_string(),
            Action::UnmuteSet { site, parameter } => format!("x|{site}|{parameter}"),
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
            ["l"] => Some(Action::LatestSites),
            ["L", site] if valid_short(site) => Some(Action::Latest((*site).to_string())),
            ["t"] => Some(Action::ThresholdSites),
            ["T", site] if valid_short(site) => Some(Action::Thresholds((*site).to_string())),
            ["b"] => Some(Action::BatterySites),
            ["B", site] if valid_short(site) => Some(Action::Battery((*site).to_string())),
            ["m"] => Some(Action::MuteSites),
            ["n", site] if valid_short(site) => Some(Action::MuteParams((*site).to_string())),
            ["w", site, parameter] if valid_short(site) && valid_short(parameter) => {
                Some(Action::MuteWhen {
                    site: (*site).to_string(),
                    parameter: (*parameter).to_string(),
                })
            }
            ["k", site, parameter, days] if valid_short(site) && valid_short(parameter) => {
                // A duration that isn't a non-negative integer is a malformed payload, not a
                // silently-defaulted one: it would otherwise mute for a length nobody chose.
                days.parse::<i64>()
                    .ok()
                    .filter(|d| *d >= 0)
                    .map(|days| Action::MuteSet {
                        site: (*site).to_string(),
                        parameter: (*parameter).to_string(),
                        days,
                    })
            }
            ["u"] => Some(Action::Muted),
            ["x", site, parameter] if valid_short(site) && valid_short(parameter) => {
                Some(Action::UnmuteSet {
                    site: (*site).to_string(),
                    parameter: (*parameter).to_string(),
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

/// How long a mute lasts, offered under the parameter picker. `0` means no expiry.
pub const MUTE_DURATIONS: [(i64, &str); 4] = [
    (1, "1 day"),
    (7, "7 days"),
    (30, "30 days"),
    (0, "No expiry"),
];

/// The final step of muting: the tap that writes.
#[must_use]
pub fn mute_duration_row(site: &str, parameter: &str) -> Vec<Button> {
    MUTE_DURATIONS
        .iter()
        .map(|(days, label)| Button {
            text: (*label).to_string(),
            data: Action::MuteSet {
                site: site.to_string(),
                parameter: parameter.to_string(),
                days: *days,
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
        let parameter = short(Uuid::parse_str("aabbccdd-0000-4000-8000-000000000009").unwrap());
        for action in every_action(&site, &parameter) {
            assert_eq!(Action::parse(&action.encode()), Some(action));
        }
    }

    /// One list, so a variant added without a wire form fails to compile here rather than becoming
    /// a silently dead button.
    fn every_action(site: &str, parameter: &str) -> Vec<Action> {
        vec![
            Action::Sites,
            Action::Parameters(site.to_string()),
            Action::Overview(site.to_string()),
            Action::View {
                site: site.to_string(),
                parameter: parameter.to_string(),
                window: "6h".to_string(),
            },
            Action::LatestSites,
            Action::Latest(site.to_string()),
            Action::ThresholdSites,
            Action::Thresholds(site.to_string()),
            Action::BatterySites,
            Action::Battery(site.to_string()),
            Action::MuteSites,
            Action::MuteParams(site.to_string()),
            Action::MuteWhen {
                site: site.to_string(),
                parameter: parameter.to_string(),
            },
            Action::MuteSet {
                site: site.to_string(),
                parameter: parameter.to_string(),
                days: 30,
            },
            Action::Muted,
            Action::UnmuteSet {
                site: site.to_string(),
                parameter: parameter.to_string(),
            },
        ]
    }

    #[test]
    fn test_every_payload_fits_telegram_limit() {
        let site = short(Uuid::new_v4());
        let parameter = short(Uuid::new_v4());
        for action in every_action(&site, &parameter) {
            let encoded = action.encode();
            assert!(
                encoded.len() <= 64,
                "{action:?} encoded to {} bytes",
                encoded.len()
            );
        }
    }

    /// A mute button carries the whole instruction, so a duration that is not a plain non-negative
    /// integer must be refused rather than defaulting to a length nobody chose.
    #[test]
    fn test_mute_rejects_a_duration_that_is_not_a_count_of_days() {
        let site = short(Uuid::new_v4());
        let parameter = short(Uuid::new_v4());
        for bad in ["-1", "7.5", "forever", "", "1e3"] {
            assert_eq!(
                Action::parse(&format!("k|{site}|{parameter}|{bad}")),
                None,
                "accepted {bad:?} as a duration"
            );
        }
        assert_eq!(
            Action::parse(&format!("k|{site}|{parameter}|0")),
            Some(Action::MuteSet {
                site,
                parameter,
                days: 0
            }),
            "0 is the no-expiry case and must parse"
        );
    }

    /// The gates a tapped button has to pass are decided here, so they are pinned here.
    #[test]
    fn test_write_and_admin_actions_are_marked() {
        let site = short(Uuid::new_v4());
        let parameter = short(Uuid::new_v4());
        let write = [
            Action::MuteSites,
            Action::MuteParams(site.clone()),
            Action::MuteWhen {
                site: site.clone(),
                parameter: parameter.clone(),
            },
            Action::MuteSet {
                site: site.clone(),
                parameter: parameter.clone(),
                days: 1,
            },
            Action::UnmuteSet {
                site: site.clone(),
                parameter,
            },
        ];
        for action in &write {
            assert!(action.is_write(), "{action:?} writes");
            assert!(action.requires_admin(), "{action:?} is administrator-only");
        }
        // Listing mutes is a read, but still administrator-only, and its buttons write.
        assert!(!Action::Muted.is_write());
        assert!(Action::Muted.requires_admin());
        for action in [Action::Sites, Action::LatestSites, Action::Latest(site)] {
            assert!(!action.is_write(), "{action:?} does not write");
            assert!(!action.requires_admin(), "{action:?} is open to any member");
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
