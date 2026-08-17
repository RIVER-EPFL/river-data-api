//! Argument parsing for the plot commands.
//!
//! Pure functions over strings: no database, no state, so the fiddly part is unit-testable without
//! a fixture. The resolution against real sites and parameters happens in `commands::plot`, which
//! walks the candidate splits this module produces.
//!
//! The hard case is that a site name may contain spaces (`Les Dailles`) while a parameter usually
//! does not, and both arrive in one undelimited string. The legacy R bot took `parts[2]` as the
//! station and so could never plot its own README's example. Three layers solve it here: an
//! explicit separator wins outright, a trailing window token is stripped, and what remains is
//! offered as candidate splits, shortest parameter first.

use chrono::Duration;

/// The window used when `/plot` is given no window token.
pub const DEFAULT_WINDOW_DAYS: i64 = 7;

/// Longest window we will plot. Beyond this the monthly rollup is too coarse to be informative and
/// the query starts to cost real time.
const MAX_WINDOW_DAYS: i64 = 3 * 365;

/// At most this many tokens are tried as the parameter side of a split. Parameters are one word in
/// practice; allowing three covers `dissolved o2` without making the search quadratic.
const MAX_PARAM_TOKENS: usize = 3;

/// Legacy shorthands from the R bot, tried only when a direct resolve finds nothing.
///
/// `resolve_parameter` matches `code`/`name` substrings and has no alias table, so `volt`,
/// `temp` and the rest resolve to nothing on their own.
const ALIASES: &[(&str, &str)] = &[
    ("volt", "battery"),
    ("v", "battery"),
    ("temp", "temperature"),
    ("cond", "conductivity"),
    ("do", "dissolved"),
    ("o2", "dissolved"),
    ("oxygen", "dissolved"),
    ("turb", "turbidity"),
    ("cdom", "cdom"),
];

/// The alias for `token`, if one exists and differs from the token itself.
#[must_use]
pub fn alias_for(token: &str) -> Option<&'static str> {
    let lower = token.trim().to_lowercase();
    ALIASES
        .iter()
        .find(|(k, _)| *k == lower)
        .map(|(_, v)| *v)
        .filter(|v| *v != lower)
}

/// Parse a window token: `90m`, `6h`, `2d`, `1w`, `3mo`.
///
/// `m` is minutes and `mo` is months, because `/30d` users reach for `m` meaning minutes far more
/// often than months, and a silently misread window produces a plausible-looking wrong chart.
#[must_use]
pub fn parse_window(s: &str) -> Option<Duration> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let digits_end = s.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let (num, unit) = s.split_at(digits_end);
    let n: i64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let d = match unit {
        "m" | "min" | "mins" => Duration::minutes(n),
        "h" | "hr" | "hrs" => Duration::hours(n),
        "d" | "day" | "days" => Duration::days(n),
        "w" | "wk" | "week" | "weeks" => Duration::days(n * 7),
        "mo" | "mon" | "month" | "months" => Duration::days(n * 30),
        "y" | "yr" | "year" | "years" => Duration::days(n * 365),
        _ => return None,
    };
    if d > Duration::days(MAX_WINDOW_DAYS) {
        return None;
    }
    Some(d)
}

/// Whether a token has the *shape* of a window (digits then letters), regardless of whether it is
/// a valid one.
///
/// A trailing `9y` must be reported as a bad window rather than silently tried as a parameter
/// name, which is what makes the error message name the real problem.
#[must_use]
pub fn looks_like_window(s: &str) -> bool {
    let s = s.trim();
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_digit()
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The window a legacy command name fixes. `/plot` carries none and the caller defaults.
#[must_use]
pub fn window_of_command(cmd: &str) -> Option<Duration> {
    match cmd {
        "plot" => None,
        other => parse_window(other),
    }
}

/// Whether `cmd` is one of the plot commands.
#[must_use]
pub fn is_plot_command(cmd: &str) -> bool {
    cmd == "plot"
        || (parse_window(cmd).is_some() && cmd.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// Split on whitespace, honouring double quotes so a site can be pinned explicitly.
#[must_use]
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// What a plot command's arguments resolved to, before hitting the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArgs {
    /// Candidate `(site, parameter)` splits, best first. Empty only when the input was unusable.
    pub candidates: Vec<(String, String)>,
    /// Present when the caller named a window; `None` means use the command's or the default.
    pub window_token: Option<String>,
    /// True when commas or quotes made the split explicit, so a failure is the user's typo rather
    /// than our guess. Drives a more confident error message.
    pub explicit: bool,
}

/// Parse the argument string of a plot command.
///
/// Returns `None` when there is nothing to work with (fewer than two tokens).
#[must_use]
pub fn parse(args: &str) -> Option<ParsedArgs> {
    let args = args.trim();
    if args.is_empty() {
        return None;
    }

    // Layer 1: an explicit separator ends the guessing. `Les Dailles, depth, 7d`.
    if args.contains(',') {
        let parts: Vec<String> = args
            .split(',')
            .map(|p| p.trim().trim_matches('"').trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() >= 2 {
            let window_token = parts.get(2).cloned();
            return Some(ParsedArgs {
                candidates: vec![(parts[0].clone(), parts[1].clone())],
                window_token,
                explicit: true,
            });
        }
        return None;
    }

    let quoted = args.contains('"');
    let mut tokens = tokenize(args);
    if tokens.len() < 2 {
        return None;
    }

    // Layer 2: a trailing window token is unambiguous, no parameter is named `7d`. Taken on
    // *shape*, not validity, so `9y` reports a bad window instead of an unknown parameter.
    let window_token = if tokens.len() > 2 && looks_like_window(tokens.last()?) {
        tokens.pop()
    } else {
        None
    };
    if tokens.len() < 2 {
        return None;
    }

    // Layer 3: candidate splits, shortest parameter first. A one-word parameter is the common
    // case, and trying it first means `Depth Station depth` resolves the site as `Depth Station`
    // rather than stopping at `Depth`.
    let candidates = candidate_splits(&tokens);
    Some(ParsedArgs {
        candidates,
        window_token,
        explicit: quoted,
    })
}

/// Every `(site, parameter)` split of `tokens`, parameter-shortest first.
#[must_use]
pub fn candidate_splits(tokens: &[String]) -> Vec<(String, String)> {
    let n = tokens.len();
    let max_param = MAX_PARAM_TOKENS.min(n - 1);
    (1..=max_param)
        .map(|param_len| {
            let split = n - param_len;
            (tokens[..split].join(" "), tokens[split..].join(" "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_units() {
        assert_eq!(parse_window("6h"), Some(Duration::hours(6)));
        assert_eq!(parse_window("90m"), Some(Duration::minutes(90)));
        assert_eq!(parse_window("2d"), Some(Duration::days(2)));
        assert_eq!(parse_window("1w"), Some(Duration::days(7)));
        assert_eq!(parse_window("3mo"), Some(Duration::days(90)));
        assert_eq!(parse_window("30D"), Some(Duration::days(30)));
    }

    #[test]
    fn window_rejects_nonsense() {
        assert_eq!(parse_window(""), None);
        assert_eq!(parse_window("abc"), None);
        assert_eq!(parse_window("d7"), None);
        assert_eq!(parse_window("0d"), None);
        assert_eq!(parse_window("7"), None);
        assert_eq!(parse_window("9y"), None, "beyond the three-year cap");
    }

    #[test]
    fn minutes_and_months_are_distinguishable() {
        // Scenario: `m` is ambiguous in the wild. Expected behaviour: `m` is minutes, `mo` months,
        // so a mistyped window fails loudly instead of drawing a plausible wrong chart.
        assert_eq!(parse_window("3m"), Some(Duration::minutes(3)));
        assert_eq!(parse_window("3mo"), Some(Duration::days(90)));
    }

    #[test]
    fn legacy_command_windows() {
        assert_eq!(window_of_command("7d"), Some(Duration::days(7)));
        assert_eq!(window_of_command("1d"), Some(Duration::days(1)));
        assert_eq!(window_of_command("30d"), Some(Duration::days(30)));
        assert_eq!(window_of_command("6h"), Some(Duration::hours(6)));
        assert_eq!(window_of_command("plot"), None);
    }

    #[test]
    fn plot_command_recognition() {
        for cmd in ["plot", "1d", "3d", "7d", "30d", "6h", "12h"] {
            assert!(is_plot_command(cmd), "{cmd} should be a plot command");
        }
        for cmd in ["latest", "status", "grab", "d7", "mute"] {
            assert!(!is_plot_command(cmd), "{cmd} should not be a plot command");
        }
    }

    #[test]
    fn multi_word_site_resolves_shortest_parameter_first() {
        // The exact case the legacy R bot could not express.
        let parsed = parse("Les Dailles depth 7d").expect("parses");
        assert_eq!(parsed.window_token.as_deref(), Some("7d"));
        assert_eq!(
            parsed.candidates.first(),
            Some(&("Les Dailles".to_string(), "depth".to_string()))
        );
    }

    #[test]
    fn candidate_order_prefers_a_one_word_parameter() {
        let tokens = vec![
            "Depth".to_string(),
            "Station".to_string(),
            "depth".to_string(),
        ];
        let splits = candidate_splits(&tokens);
        assert_eq!(
            splits[0],
            ("Depth Station".to_string(), "depth".to_string()),
            "a site whose name contains a parameter word must still resolve"
        );
        assert_eq!(
            splits[1],
            ("Depth".to_string(), "Station depth".to_string())
        );
    }

    #[test]
    fn comma_form_needs_no_guessing() {
        let parsed = parse("Les Dailles, depth, 7d").expect("parses");
        assert!(parsed.explicit);
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(
            parsed.candidates[0],
            ("Les Dailles".to_string(), "depth".to_string())
        );
        assert_eq!(parsed.window_token.as_deref(), Some("7d"));
    }

    #[test]
    fn comma_form_without_a_window() {
        let parsed = parse("Saxon, turbidity").expect("parses");
        assert_eq!(parsed.window_token, None);
        assert_eq!(parsed.candidates[0].1, "turbidity");
    }

    #[test]
    fn quoted_site_is_explicit() {
        let parsed = parse("\"Les Dailles\" depth 7d").expect("parses");
        assert!(parsed.explicit);
        assert_eq!(
            parsed.candidates.first(),
            Some(&("Les Dailles".to_string(), "depth".to_string()))
        );
    }

    #[test]
    fn a_trailing_non_window_token_stays_part_of_the_parameter() {
        let parsed = parse("Saxon dissolved o2").expect("parses");
        assert_eq!(parsed.window_token, None);
        assert_eq!(
            parsed.candidates.first(),
            Some(&("Saxon dissolved".to_string(), "o2".to_string()))
        );
        assert!(
            parsed
                .candidates
                .contains(&("Saxon".to_string(), "dissolved o2".to_string())),
            "the two-token parameter must remain a candidate"
        );
    }

    #[test]
    fn two_tokens_do_not_lose_the_parameter_to_a_window() {
        // `/7d Saxon depth` leaves exactly two tokens; neither may be eaten as a window.
        let parsed = parse("Saxon depth").expect("parses");
        assert_eq!(parsed.window_token, None);
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(
            parsed.candidates[0],
            ("Saxon".to_string(), "depth".to_string())
        );
    }

    #[test]
    fn too_few_tokens_is_none() {
        assert!(parse("").is_none());
        assert!(parse("Saxon").is_none());
        assert!(parse("   ").is_none());
    }

    #[test]
    fn aliases_cover_the_legacy_shorthands() {
        assert_eq!(alias_for("volt"), Some("battery"));
        assert_eq!(alias_for("VOLT"), Some("battery"));
        assert_eq!(alias_for("turb"), Some("turbidity"));
        assert_eq!(alias_for("cond"), Some("conductivity"));
        assert_eq!(alias_for("depth"), None, "already resolves directly");
        assert_eq!(alias_for("cdom"), None, "alias equals the token");
    }

    #[test]
    fn an_out_of_range_window_is_reported_as_a_window() {
        // Scenario: `/plot Upstream depth 9y`. Expected behaviour: the trailing token is taken as
        // the window so the reply names the window, not a missing parameter called "9y".
        let parsed = parse("Upstream depth 9y").expect("parses");
        assert_eq!(parsed.window_token.as_deref(), Some("9y"));
        assert!(parse_window("9y").is_none());
    }

    #[test]
    fn window_shape_detection() {
        assert!(looks_like_window("7d"));
        assert!(looks_like_window("9y"));
        assert!(looks_like_window("3mo"));
        assert!(!looks_like_window("depth"));
        assert!(!looks_like_window("d7"));
        assert!(!looks_like_window("7"));
        assert!(!looks_like_window(""));
    }

    #[test]
    fn tokenize_handles_quotes_and_runs_of_space() {
        assert_eq!(tokenize("a  b\tc"), vec!["a", "b", "c"]);
        assert_eq!(tokenize("\"a b\" c"), vec!["a b", "c"]);
    }
}
