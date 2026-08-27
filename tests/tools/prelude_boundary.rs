//! Pins the shape the authoring editor relies on to tell the vendored portal functions apart from
//! the authored wrapper inside one stored script.
//!
//! The editor has no copy of `prelude.R`: a seeded version keeps whichever revision was current when
//! the database was first migrated, and the seed migration never revisits it, so a text or hash
//! match would go stale one prelude edit later. It reads the prelude's own provenance lines instead.
//! This test asserts the vendored file and every wrapper keep that readable: the rule below is the
//! same one `river-data-ui/src/lib/components/tools/preludeBoundary.ts` applies.

const PRELUDE: &str = include_str!("../../migration/tool_seed/prelude.R");
const VENDOR_MARKER: &str = "# Source: cnet-data-portal";

const WRAPPERS: &[(&str, &str)] = &[
    (
        "alkalinity",
        include_str!("../../migration/tool_seed/alkalinity/wrapper.R"),
    ),
    (
        "benthic",
        include_str!("../../migration/tool_seed/benthic/wrapper.R"),
    ),
    (
        "chlorophyll",
        include_str!("../../migration/tool_seed/chlorophyll/wrapper.R"),
    ),
    (
        "co2_air",
        include_str!("../../migration/tool_seed/co2_air/wrapper.R"),
    ),
    (
        "dic",
        include_str!("../../migration/tool_seed/dic/wrapper.R"),
    ),
    (
        "discharge",
        include_str!("../../migration/tool_seed/discharge/wrapper.R"),
    ),
    (
        "doc",
        include_str!("../../migration/tool_seed/doc/wrapper.R"),
    ),
    (
        "dom",
        include_str!("../../migration/tool_seed/dom/wrapper.R"),
    ),
    (
        "field_data",
        include_str!("../../migration/tool_seed/field_data/wrapper.R"),
    ),
    (
        "nutrients",
        include_str!("../../migration/tool_seed/nutrients/wrapper.R"),
    ),
    (
        "pco2",
        include_str!("../../migration/tool_seed/pco2/wrapper.R"),
    ),
    (
        "tss_afdm",
        include_str!("../../migration/tool_seed/tss_afdm/wrapper.R"),
    ),
];

/// Lines the vendored prelude occupies at the top of `script`, 0 when it opens with none.
fn prelude_line_count(script: &str) -> usize {
    let lines: Vec<&str> = script.split('\n').collect();
    if !lines.first().is_some_and(|l| l.starts_with(VENDOR_MARKER)) {
        return 0;
    }
    let Some(last_marker) = lines.iter().rposition(|l| l.starts_with(VENDOR_MARKER)) else {
        return 0;
    };
    let Some(close) = lines[last_marker + 1..].iter().position(|l| *l == "}") else {
        return 0;
    };
    let mut end = last_marker + 1 + close;
    while end + 1 < lines.len() && lines[end + 1].trim().is_empty() {
        end += 1;
    }
    if end + 1 >= lines.len() { 0 } else { end + 1 }
}

#[test]
fn every_seeded_script_reports_its_prelude_boundary() {
    for (name, wrapper) in WRAPPERS {
        let script = migration::tool_prelude::script_for(PRELUDE, wrapper);
        let detected = prelude_line_count(&script);
        let vendored = script
            .strip_suffix(wrapper)
            .expect("the wrapper is the tail of the script");

        if vendored.trim().is_empty() {
            // A tool that calls none of the portal functions ships none of them, and the editor
            // is right to show the whole script as authored.
            assert_eq!(detected, 0, "{name}: {detected} prelude lines out of none");
            continue;
        }
        assert_eq!(
            detected,
            vendored.split('\n').count() - 1,
            "{name}: prelude boundary landed on line {detected}"
        );
        let first_authored = script.split('\n').nth(detected).unwrap_or("");
        assert!(
            !first_authored.trim().is_empty(),
            "{name}: the authored region opens on a blank line"
        );
    }
}

#[test]
fn a_wrapper_never_carries_the_vendor_marker() {
    // The boundary is the last vendored provenance line, so one inside a wrapper would push the
    // fold over the author's own code.
    for (name, wrapper) in WRAPPERS {
        assert!(
            !wrapper.contains(VENDOR_MARKER),
            "{name}: wrapper carries the vendored provenance line"
        );
    }
}

#[test]
fn a_script_written_from_scratch_reports_no_prelude() {
    let script = "tool <- function(inputs, constants, curves) {\n  list(value = inputs$a)\n}\n";
    assert_eq!(prelude_line_count(script), 0);
}
