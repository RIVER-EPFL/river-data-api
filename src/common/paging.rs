//! One page window, and one `Content-Range`, for the lists that are not entity routers.
//!
//! The thirty CRUD routers page through crudcrate. Everything hand-written here reached for its own
//! clamp and its own header, in two client vocabularies: `page`/`page_size` and `limit`/`offset`.
//! Both name the same window, so both build the same [`Window`], and the header is built once from
//! crudcrate's own function, which sanitises the resource name and clamps the end.

use axum::http::HeaderMap;
use serde::Serialize;
use utoipa::ToSchema;

/// The rows a request asked for: where the page starts and how many it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub offset: u64,
    pub limit: u64,
}

impl Window {
    /// A 1-based `page` with a `page_size`, the vocabulary the portal's lists use. A page below 1
    /// is page 1: a caller counting from zero asks for the first page, not for nothing.
    #[must_use]
    pub fn from_page(page: Option<u64>, page_size: Option<u64>, default: u64, max: u64) -> Self {
        let limit = page_size.unwrap_or(default).clamp(1, max);
        let offset = page.unwrap_or(1).max(1).saturating_sub(1).saturating_mul(limit);
        Self { offset, limit }
    }

    /// A `limit` with an `offset`, the vocabulary the feeds use.
    #[must_use]
    pub fn from_limit_offset(
        limit: Option<u64>,
        offset: Option<u64>,
        default: u64,
        max: u64,
    ) -> Self {
        Self {
            offset: offset.unwrap_or(0),
            limit: limit.unwrap_or(default).clamp(1, max),
        }
    }

    /// A React-Admin `range=[start,end]` with an inclusive end, the vocabulary the admin lists
    /// inherited. An unparseable range is the first page, which is what the caller's client renders
    /// while it works out what it meant.
    #[must_use]
    pub fn from_range(range: Option<&str>, default: u64, max: u64) -> Self {
        let Some(range) = range else {
            return Self { offset: 0, limit: default };
        };
        let (start, end) = crudcrate::parse_range(Some(range.to_string()));
        Self {
            offset: start,
            limit: end
                .saturating_sub(start)
                .saturating_add(1)
                .clamp(1, max),
        }
    }

    /// The 1-based page this window is, for a response that reports one.
    #[must_use]
    pub fn page(self) -> u64 {
        self.offset / self.limit + 1
    }
}

/// One page of rows and nothing else: the shape every list answers in when it has no subject of
/// its own.
///
/// A list that carries a subject beside its rows is a domain object, not a page, and keeps its own
/// name for them: `GET /sites/{id}/visits` answers with the site and the grid's columns,
/// `/streams/{id}/receipts` with the stream, the hold list with its per-kind counts, the search
/// with results grouped by entity, and the alarm summary with its breakdowns. Those stay as they
/// are; folding a subject into an envelope would only move it to a second key.
#[derive(Debug, Serialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Rows matching the filter, not rows on this page.
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

impl<T> Page<T> {
    /// The page `window` asked for, with the total the count query answered. An absent window is a
    /// caller that asked for everything, which is page 1 of one page.
    #[must_use]
    pub fn new(items: Vec<T>, total: u64, window: Option<Window>) -> Self {
        Self {
            items,
            total,
            page: window.map_or(1, Window::page),
            page_size: window.map_or(total, |w| w.limit),
        }
    }
}

/// `Content-Range: {resource} {start}-{end}/{total}` with an inclusive end, and the CORS expose
/// header a browser needs to read it.
///
/// A page that returned nothing reports `*/{total}`, RFC 7233's unsatisfied-range form: a request
/// past the last row has no first row to name, and `{offset}-{offset}` would name one.
#[must_use]
pub fn content_range(offset: u64, returned: usize, total: u64, resource: &str) -> HeaderMap {
    let mut headers = if returned == 0 {
        let mut headers = HeaderMap::new();
        if let Ok(value) = format!("{resource} */{total}").parse() {
            headers.insert("Content-Range", value);
        }
        headers
    } else {
        crudcrate::calculate_content_range(offset, returned as u64, total, resource)
    };
    if let Ok(value) = "Content-Range".parse() {
        headers.insert("Access-Control-Expose-Headers", value);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(headers: &HeaderMap) -> String {
        headers["Content-Range"].to_str().unwrap().to_string()
    }

    #[test]
    fn test_from_page_counts_from_one() {
        let w = Window::from_page(Some(2), Some(10), 50, 100);
        assert_eq!(w, Window { offset: 10, limit: 10 });
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
        assert_eq!(w, Window { offset: 40, limit: 1000 });
        assert_eq!(Window::from_limit_offset(None, None, 200, 1000).limit, 200);
    }

    #[test]
    fn test_from_range_reads_an_inclusive_end_and_clamps_it() {
        assert_eq!(
            Window::from_range(Some("[10,19]"), 25, 100),
            Window { offset: 10, limit: 10 }
        );
        assert_eq!(Window::from_range(Some("[0,9999]"), 25, 100).limit, 100);
        assert_eq!(
            Window::from_range(None, 25, 100),
            Window { offset: 0, limit: 25 }
        );
        assert_eq!(Window::from_range(Some("nonsense"), 25, 100).offset, 0);
    }

    #[test]
    fn a_page_reports_the_window_it_answered_and_the_total_it_counted() {
        let page = Page::new(vec![1, 2, 3], 30, Some(Window::from_page(Some(2), Some(3), 50, 100)));
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
}
