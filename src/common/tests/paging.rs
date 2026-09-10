use super::*;

fn range(headers: &HeaderMap) -> String {
    headers["Content-Range"].to_str().unwrap().to_string()
}

#[test]
fn test_from_page_counts_from_one() {
    let w = Window::from_page(Some(2), Some(10), 50, 100);
    assert_eq!(
        w,
        Window {
            offset: 10,
            limit: 10
        }
    );
    assert_eq!(w.page(), 2);
}

#[test]
fn test_from_page_treats_page_zero_as_the_first_page() {
    assert_eq!(Window::from_page(Some(0), Some(5), 50, 100).offset, 0);
    assert_eq!(Window::from_page(None, Some(5), 50, 100).offset, 0);
}

#[test]
fn test_from_page_clamps_the_size_both_ways() {
    assert_eq!(Window::from_page(Some(1), Some(500), 50, 100).limit, 100);
    assert_eq!(Window::from_page(Some(1), Some(0), 50, 100).limit, 1);
    assert_eq!(Window::from_page(Some(1), None, 50, 100).limit, 50);
}

#[test]
fn test_from_limit_offset_clamps_the_limit_and_keeps_the_offset() {
    let w = Window::from_limit_offset(Some(9999), Some(40), 200, 1000);
    assert_eq!(
        w,
        Window {
            offset: 40,
            limit: 1000
        }
    );
    assert_eq!(Window::from_limit_offset(None, None, 200, 1000).limit, 200);
}

#[test]
fn test_from_range_reads_an_inclusive_end_and_clamps_it() {
    assert_eq!(
        Window::from_range(Some("[10,19]"), 25, 100),
        Window {
            offset: 10,
            limit: 10
        }
    );
    assert_eq!(Window::from_range(Some("[0,9999]"), 25, 100).limit, 100);
    assert_eq!(
        Window::from_range(None, 25, 100),
        Window {
            offset: 0,
            limit: 25
        }
    );
    assert_eq!(Window::from_range(Some("nonsense"), 25, 100).offset, 0);
}

#[test]
fn a_page_reports_the_window_it_answered_and_the_total_it_counted() {
    let page = Page::new(
        vec![1, 2, 3],
        30,
        Some(Window::from_page(Some(2), Some(3), 50, 100)),
    );
    assert_eq!((page.page, page.page_size, page.total), (2, 3, 30));
}

#[test]
fn an_unpaged_list_is_one_page_holding_everything() {
    let page = Page::new(vec![1, 2], 2, None);
    assert_eq!((page.page, page.page_size), (1, 2));
}

#[test]
fn test_content_range_reports_an_inclusive_end() {
    assert_eq!(range(&content_range(10, 10, 30, "items")), "items 10-19/30");
}

#[test]
fn test_content_range_past_the_last_row_names_no_row() {
    assert_eq!(range(&content_range(90, 0, 30, "items")), "items */30");
    assert_eq!(range(&content_range(0, 0, 0, "users")), "users */0");
}

#[test]
fn test_content_range_exposes_itself_to_a_browser() {
    let headers = content_range(0, 5, 30, "items");
    assert_eq!(headers["Access-Control-Expose-Headers"], "Content-Range");
}
