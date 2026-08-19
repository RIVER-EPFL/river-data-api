//! Cuts the vendored portal calculation functions down to the ones a wrapper actually reaches.
//!
//! `tool_seed/prelude.R` is the whole CNET/METALP `calculation_functions.R`, 26 functions over
//! ~1,180 lines, and it is vendored verbatim so the fidelity claim is a byte comparison. Shipping
//! all of it as every tool's script made the stored script mostly other tools' arithmetic: the
//! alkalinity script was 1,184 lines to call one 19-line function, and someone reading a version
//! in the editor had to find the wrapper at the bottom.
//!
//! So the stored script carries the wrapper plus the transitive closure of the prelude functions
//! it calls, in the prelude's own order and with each function's text unchanged. Selecting is not
//! rewriting: every line that ships is still a line of the vendored file, so the comparison that
//! proves fidelity is unaffected.
//!
//! Reachability is by name over the whole text, which over-includes (a function named in a comment
//! or a string is kept) and cannot under-include for these scripts, which never build a function
//! name at run time. Over-inclusion costs a few unused lines; under-inclusion would be a runtime
//! "could not find function", so the bias is deliberate.

/// One top-level `name <- function(...)` definition, with the comment block above it.
struct Block<'a> {
    name: &'a str,
    text: String,
}

fn definition_name(line: &str) -> Option<&str> {
    let mut end = 0;
    for (i, c) in line.char_indices() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    let name = &line[..end];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix("<-").or_else(|| rest.strip_prefix('='))?;
    rest.trim_start().starts_with("function").then_some(name)
}

/// Whether `name` occurs in `text` as a whole word rather than inside a longer identifier.
fn mentions(text: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_');
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(hit) = text[from..].find(name) {
        let start = from + hit;
        let end = start + name.len();
        let before_ok = start == 0 || boundary(bytes[start - 1] as char);
        let after_ok = end == text.len() || boundary(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn blocks(prelude: &str) -> Vec<Block<'_>> {
    let lines: Vec<&str> = prelude.lines().collect();
    let defs: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| definition_name(l).map(|n| (i, n)))
        .collect();

    defs.iter()
        .enumerate()
        .map(|(k, &(line_no, name))| {
            // The comment run above a definition documents it, and in the vendored file that is
            // where the provenance header sits, so it travels with the function. A blank line
            // separates the two, so the walk crosses blanks but only claims them when a comment
            // run really does sit above.
            let mut start = line_no;
            let mut scan = line_no;
            while scan > 0
                && (lines[scan - 1].trim().is_empty() || lines[scan - 1].starts_with('#'))
            {
                scan -= 1;
                if lines[scan].starts_with('#') {
                    start = scan;
                }
            }
            let mut end = defs.get(k + 1).map_or(lines.len(), |&(next, _)| next);
            while end > line_no && {
                let l = lines[end - 1];
                l.trim().is_empty() || l.starts_with('#')
            } {
                end -= 1;
            }
            Block {
                name,
                text: lines[start..end].join("\n"),
            }
        })
        .collect()
}

/// The wrapper preceded by exactly the prelude functions it reaches, transitively.
pub fn script_for(prelude: &str, wrapper: &str) -> String {
    let blocks = blocks(prelude);

    let mut keep = vec![false; blocks.len()];
    for (i, b) in blocks.iter().enumerate() {
        keep[i] = mentions(wrapper, b.name);
    }
    // A kept function's own calls are equally part of the script, and one pass over the set is not
    // enough because a callee may sit above its caller.
    loop {
        let mut grew = false;
        for i in 0..blocks.len() {
            if keep[i] {
                continue;
            }
            let referenced = blocks
                .iter()
                .zip(&keep)
                .any(|(b, &k)| k && b.name != blocks[i].name && mentions(&b.text, blocks[i].name));
            if referenced {
                keep[i] = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut out = String::new();
    for (b, &k) in blocks.iter().zip(&keep) {
        if k {
            out.push_str(&b.text);
            out.push_str("\n\n");
        }
    }
    out.push_str(wrapper);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRELUDE: &str = include_str!("../tool_seed/prelude.R");

    #[test]
    fn a_functions_own_calls_travel_with_it() {
        let prelude = "\
a <- function() 1
# doc for b
b <- function() a() + 1
c <- function() 99
";
        let script = script_for(prelude, "tool <- function() b()");
        assert!(script.contains("b <- function"), "{script}");
        assert!(
            script.contains("a <- function"),
            "the callee of b: {script}"
        );
        assert!(!script.contains("c <- function"), "unreached: {script}");
        assert!(
            script.contains("# doc for b"),
            "the comment above b: {script}"
        );
    }

    #[test]
    fn a_longer_identifier_is_not_a_call() {
        let prelude = "calcSd <- function() 1\ncalcSdX <- function() 2\n";
        let script = script_for(prelude, "tool <- function() calcSdX()");
        assert!(script.contains("calcSdX <- function"));
        assert!(!script.contains("\ncalcSd <- function"), "{script}");
    }

    #[test]
    fn a_kept_function_keeps_the_provenance_header_above_it() {
        // The header is what says which portal file and lines the function came from, and it is
        // separated from the definition by a blank line, so a walk that stops at the first
        // non-comment line drops exactly the provenance.
        let script = script_for(PRELUDE, include_str!("../tool_seed/tss_afdm/wrapper.R"));
        assert_eq!(
            script.matches("# Source: cnet-data-portal").count(),
            2,
            "one header per kept function: {script}"
        );
        assert!(script.starts_with("# Source: cnet-data-portal"));
    }

    #[test]
    fn every_kept_line_is_a_line_of_the_vendored_prelude() {
        // Selecting is not rewriting: the fidelity claim rests on the shipped text being the
        // portal's own, so a trimmed script may drop lines and never alter one.
        let wrapper = include_str!("../tool_seed/pco2/wrapper.R");
        let script = script_for(PRELUDE, wrapper);
        let kept = script.strip_suffix(wrapper).expect("wrapper is the tail");
        for line in kept.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                PRELUDE.lines().any(|p| p == line),
                "not a vendored line: {line}"
            );
        }
    }

    #[test]
    fn every_seeded_tool_ships_every_function_it_names() {
        let seeds: &[(&str, &str)] = &[
            (
                "alkalinity",
                include_str!("../tool_seed/alkalinity/wrapper.R"),
            ),
            ("benthic", include_str!("../tool_seed/benthic/wrapper.R")),
            (
                "chla_benthic",
                include_str!("../tool_seed/chla_benthic/wrapper.R"),
            ),
            (
                "chlorophyll",
                include_str!("../tool_seed/chlorophyll/wrapper.R"),
            ),
            ("co2_air", include_str!("../tool_seed/co2_air/wrapper.R")),
            ("dic", include_str!("../tool_seed/dic/wrapper.R")),
            (
                "discharge",
                include_str!("../tool_seed/discharge/wrapper.R"),
            ),
            ("doc", include_str!("../tool_seed/doc/wrapper.R")),
            ("dom", include_str!("../tool_seed/dom/wrapper.R")),
            (
                "field_data",
                include_str!("../tool_seed/field_data/wrapper.R"),
            ),
            (
                "nutrients",
                include_str!("../tool_seed/nutrients/wrapper.R"),
            ),
            ("pco2", include_str!("../tool_seed/pco2/wrapper.R")),
            ("tss_afdm", include_str!("../tool_seed/tss_afdm/wrapper.R")),
        ];
        let all = blocks(PRELUDE);
        assert_eq!(all.len(), 26, "the vendored prelude defines 26 functions");

        for (name, wrapper) in seeds {
            let script = script_for(PRELUDE, wrapper);
            for b in &all {
                if mentions(&script, b.name) {
                    assert!(
                        script.contains(&format!("{} <- function", b.name)),
                        "{name} calls {} without shipping it",
                        b.name
                    );
                }
            }
            assert!(script.ends_with(wrapper), "{name} keeps its wrapper");
        }
    }
}
