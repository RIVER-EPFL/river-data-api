use crate::routes::private::tools::service::parse_tool_error;

#[test]
fn opencpu_plain_text_is_not_a_tool_error() {
    let body = "unused argument (nope = 1)\n\nIn call:\nrun_tool(nope = 1L)";
    assert!(parse_tool_error(body).is_none());
}

#[test]
fn the_marker_line_carries_message_call_and_traceback() {
    let body = concat!(
        r#"{"error":"tool_error","message":"boom","call":"fn(x)","traceback":["fn(x)"]}"#,
        "\nBacktrace:\n  1. eval(call)"
    );
    let parsed = parse_tool_error(body).expect("first line parses");
    assert_eq!(parsed.message, "boom");
    assert_eq!(parsed.call.as_deref(), Some("fn(x)"));
    assert_eq!(parsed.traceback, vec!["fn(x)".to_string()]);
}

#[test]
fn a_json_line_without_the_marker_falls_through() {
    let body = r#"{"error":"other","message":"boom"}"#;
    assert!(parse_tool_error(body).is_none());
}
