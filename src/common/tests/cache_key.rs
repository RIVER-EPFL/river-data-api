use super::*;
use serde::Serialize;

#[derive(Serialize)]
struct Query {
    parameter_ids: Option<Vec<String>>,
    split_by_sensor: Option<bool>,
}

#[derive(Serialize)]
struct Key<'a> {
    site_id: &'a str,
    format: &'a str,
    #[serde(flatten)]
    query: &'a Query,
}

fn key(site: &str, format: &str, parameter_ids: Option<Vec<&str>>, split: Option<bool>) -> String {
    let query = Query {
        parameter_ids: parameter_ids.map(|ids| ids.into_iter().map(str::to_string).collect()),
        split_by_sensor: split,
    };
    key_for(
        "readings",
        &Key {
            site_id: site,
            format,
            query: &query,
        },
    )
}

#[test]
fn test_key_carries_the_prefix_and_every_field() {
    let k = key("S1", "json", Some(vec!["P1"]), None);
    assert!(k.starts_with("readings:"));
    assert!(k.contains("site_id=\"S1\""));
    assert!(k.contains("format=\"json\""));
    assert!(k.contains("parameter_ids=[\"P1\"]"));
    assert!(k.contains("split_by_sensor=null"));
}

#[test]
fn test_a_flattened_query_field_separates_keys() {
    assert_ne!(
        key("S1", "json", Some(vec!["P1"]), None),
        key("S1", "json", Some(vec!["P2"]), None)
    );
    assert_ne!(
        key("S1", "json", None, Some(true)),
        key("S1", "json", None, Some(false))
    );
    assert_ne!(
        key("S1", "json", None, None),
        key("S1", "json", None, Some(false))
    );
}

#[test]
fn test_absent_and_empty_are_different_keys() {
    assert_ne!(
        key("S1", "json", None, None),
        key("S1", "json", Some(vec![]), None)
    );
    assert_ne!(
        key("S1", "json", Some(vec![]), None),
        key("S1", "json", Some(vec![""]), None)
    );
}

#[test]
fn test_the_same_request_yields_the_same_key() {
    assert_eq!(
        key("S1", "json", Some(vec!["P1", "P2"]), Some(true)),
        key("S1", "json", Some(vec!["P1", "P2"]), Some(true))
    );
}

#[test]
fn test_list_order_is_part_of_the_key() {
    assert_ne!(
        key("S1", "json", Some(vec!["P1", "P2"]), None),
        key("S1", "json", Some(vec!["P2", "P1"]), None)
    );
}

#[test]
fn test_fields_are_emitted_in_name_order() {
    #[derive(Serialize)]
    struct Declared {
        zulu: u8,
        alpha: u8,
    }
    assert_eq!(
        key_for("p", &Declared { zulu: 1, alpha: 2 }),
        "p:alpha=2:zulu=1"
    );
}

#[test]
fn test_a_string_field_cannot_forge_another_field() {
    #[derive(Serialize)]
    struct One {
        a: String,
        b: String,
    }
    assert_ne!(
        key_for(
            "p",
            &One {
                a: "x:b=y".to_string(),
                b: String::new()
            }
        ),
        key_for(
            "p",
            &One {
                a: "x".to_string(),
                b: "y".to_string()
            }
        )
    );
}

#[test]
fn test_a_bare_value_is_one_component() {
    assert_eq!(key_for("p", "solo"), "p:\"solo\"");
}
