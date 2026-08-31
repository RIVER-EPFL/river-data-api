//! Which divisor a replicate group's standard deviation uses, and where that decision came from.
//!
//! `sample` is the n-1 divisor, `population` the n one. The source portals stored both at
//! different times, row by row inside one stream, so nothing here infers a convention from the
//! data: a slot's estimator is declared by a person (`site_parameters.sd_estimator`) or by the
//! stream's registered spec, and an undeclared slot is recorded as such rather than quietly
//! treated as `sample`.
//!
//! Every `samples` row carries both the estimator it was computed with and the [`Source`] that
//! chose it. `Source::Default` is the undeclared state: the fallback served a number because one
//! had to be served, and that row is what [the undeclared report](super::super::admin) lists and
//! what arms the audit gate.

use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

pub const SAMPLE: &str = "sample";
pub const POPULATION: &str = "population";

/// Where a sample's estimator came from, most specific first. Stored on the row beside the value
/// it chose, so "computed under no declaration" stays distinguishable from "declared sample".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A person chose it for this one collection group (an audit resolution scoped to the instant).
    Sample,
    /// A tool's manifest fixed it, or its operator chose it for this run.
    Tool,
    /// The stream's registered replicate spec declares it.
    Stream,
    /// The slot declares it: `site_parameters.sd_estimator`.
    Slot,
    /// Nothing declared one. The fallback applied and this row is undeclared.
    Default,
}

impl Source {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sample => "sample",
            Self::Tool => "tool",
            Self::Stream => "stream",
            Self::Slot => "slot",
            Self::Default => "default",
        }
    }
}

/// A resolved estimator: the value a sample is computed with, and what chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub estimator: &'static str,
    pub source: Source,
}

impl Resolved {
    /// The undeclared fallback: a sample sd, recorded as chosen by nothing.
    #[must_use]
    pub const fn undeclared() -> Self {
        Self {
            estimator: SAMPLE,
            source: Source::Default,
        }
    }

    #[must_use]
    pub const fn is_declared(&self) -> bool {
        !matches!(self.source, Source::Default)
    }
}

/// Reject anything outside the two divisors. A stored estimator is a specification, so an unknown
/// value is refused at the edge rather than falling back to one of them.
pub fn parse(value: &str) -> AppResult<&'static str> {
    match value {
        SAMPLE => Ok(SAMPLE),
        POPULATION => Ok(POPULATION),
        other => Err(AppError::BadRequest(format!(
            "unknown sd estimator '{other}'; expected 'sample' or 'population'"
        ))),
    }
}

/// The same check for an optional field.
pub fn parse_opt(value: Option<&str>) -> AppResult<Option<&'static str>> {
    value.map(parse).transpose()
}

/// The slot's declaration, or None when the slot has not declared one.
pub async fn slot_declaration<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<&'static str>> {
    let row = conn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sd_estimator FROM site_parameters
             WHERE site_id = $1 AND parameter_id = $2 AND sd_estimator IS NOT NULL
             LIMIT 1",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let stored: Option<String> = row.try_get("", "sd_estimator")?;
    // A value outside the two is not reachable through the CHECK constraint; treat it as
    // undeclared rather than failing a read.
    Ok(stored.as_deref().and_then(|v| match v {
        SAMPLE => Some(SAMPLE),
        POPULATION => Some(POPULATION),
        _ => None,
    }))
}

/// Resolve one slot's estimator, most specific wins: an explicit request value, then the stream's
/// spec, then the slot's declaration, then the undeclared fallback.
///
/// `explicit` carries its own [`Source`] because the two callers that supply one mean different
/// things by it (a tool run versus an operator's decision about one instant).
pub async fn resolve<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    explicit: Option<(&'static str, Source)>,
    stream_spec: Option<&'static str>,
) -> AppResult<Resolved> {
    if let Some((estimator, source)) = explicit {
        return Ok(Resolved { estimator, source });
    }
    if let Some(estimator) = stream_spec {
        return Ok(Resolved {
            estimator,
            source: Source::Stream,
        });
    }
    if let Some(estimator) = slot_declaration(conn, site_id, parameter_id).await? {
        return Ok(Resolved {
            estimator,
            source: Source::Slot,
        });
    }
    Ok(Resolved::undeclared())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_estimator_is_refused_rather_than_defaulted() {
        assert!(parse("populaton").is_err());
        assert!(parse("").is_err());
        assert_eq!(parse(POPULATION).unwrap(), POPULATION);
        assert_eq!(parse_opt(None).unwrap(), None);
    }

    #[test]
    fn the_fallback_is_a_sample_sd_that_reads_as_undeclared() {
        let r = Resolved::undeclared();
        assert_eq!(r.estimator, SAMPLE);
        assert_eq!(r.source.as_str(), "default");
        assert!(!r.is_declared());
    }
}
