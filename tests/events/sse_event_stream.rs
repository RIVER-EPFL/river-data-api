
use axum::body::Body;
use http_body_util::BodyExt;
use river_db::common::AppEvent;
use serial_test::serial;
use tower::ServiceExt;
use uuid::Uuid;

async fn setup() -> (
    axum::Router,
    String,
    river_db::common::EventSender,
    sea_orm::DatabaseConnection,
) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_read_data_only(&db).await;
    let (app, events) = crate::common::build_test_app_with_events(db.clone());
    (app, token, events, db)
}

/// Parse SSE frames from raw response text.
/// Returns Vec<(event_type, data_json)> for each `event:` + `data:` pair.
fn parse_sse_frames(text: &str) -> Vec<(String, serde_json::Value)> {
    let mut frames = Vec::new();
    let mut current_event: Option<String> = None;

    for line in text.lines() {
        if let Some(ev) = line.strip_prefix("event:") {
            current_event = Some(ev.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:") {
            if let Some(ev) = current_event.take() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                    frames.push((ev, json));
                }
            }
        }
    }
    frames
}

#[tokio::test]
#[serial]
async fn sse_receives_job_created_event() {
    let (app, token, events, _db) = setup().await;

    let job_id = Uuid::new_v4();

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();

    // The SSE response is streaming. We send the request, then inject an event
    // into the broadcast channel, then collect what we can from the body.
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // Send the event after the SSE stream is open
    let _ = events.send(AppEvent::JobCreated { job_id });

    // Collect body with a timeout so we don't hang forever waiting for a stream
    // that never closes. The keep-alive is 15s; we give it 2s which is enough to
    // receive the first event.
    let body_bytes = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        response.into_body().collect(),
    )
    .await;

    // Timeout is expected -- SSE streams don't close. We take whatever data arrived.
    let text = match body_bytes {
        Ok(Ok(collected)) => String::from_utf8_lossy(&collected.to_bytes()).to_string(),
        Ok(Err(e)) => panic!("body collection error: {e}"),
        Err(_) => {
            // Timeout: the stream is still open. We need to read frame-by-frame
            // instead. This path means `.collect()` blocked. Fall through to the
            // frame-based approach below.
            String::new()
        }
    };

    // If collect() returned data (the server closed or we got enough), check it
    if !text.is_empty() {
        let frames = parse_sse_frames(&text);
        assert!(
            frames.iter().any(|(ev, data)| {
                ev == "job_created"
                    && data["type"] == "job_created"
                    && data["job_id"] == job_id.to_string()
            }),
            "expected job_created event with matching job_id in SSE frames: {text}"
        );
        return;
    }

    // If collect() timed out, use the frame-by-frame approach: make a new request
    // but this time read frames individually from the body stream.
    let req2 = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();

    let response2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(response2.status().as_u16(), 200);

    let mut body = response2.into_body();

    // Send event after subscription
    let _ = events.send(AppEvent::JobCreated { job_id });

    // Read individual frames from the streaming body
    let mut accumulated = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    loop {
        let frame_result = tokio::time::timeout_at(deadline, body.frame()).await;
        match frame_result {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    accumulated.push_str(&String::from_utf8_lossy(data));
                    let frames = parse_sse_frames(&accumulated);
                    if frames
                        .iter()
                        .any(|(ev, data)| ev == "job_created" && data["type"] == "job_created")
                    {
                        // Verify the full content
                        let (_, json) = frames
                            .iter()
                            .find(|(ev, _)| ev == "job_created")
                            .unwrap();
                        assert_eq!(json["job_id"], job_id.to_string());
                        return;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("frame error: {e}"),
            Ok(None) => break, // stream ended
            Err(_) => break,   // timeout
        }
    }

    panic!(
        "did not receive job_created event within timeout. accumulated: {accumulated}"
    );
}

#[tokio::test]
#[serial]
async fn sse_receives_data_ingested_event() {
    let (app, token, events, _db) = setup().await;

    let stream_id = Uuid::new_v4();
    let site_id = Uuid::new_v4();
    let parameter_id = Uuid::new_v4();

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let mut body = response.into_body();

    // Send event after the stream is open
    let _ = events.send(AppEvent::DataIngested {
        site_id: Some(site_id),
        parameter_id: Some(parameter_id),
        stream_id,
        count: 42,
    });

    let mut accumulated = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    loop {
        let frame_result = tokio::time::timeout_at(deadline, body.frame()).await;
        match frame_result {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    accumulated.push_str(&String::from_utf8_lossy(data));
                    let frames = parse_sse_frames(&accumulated);
                    if let Some((_, json)) =
                        frames.iter().find(|(ev, _)| ev == "data_ingested")
                    {
                        assert_eq!(json["type"], "data_ingested");
                        assert_eq!(json["stream_id"], stream_id.to_string());
                        assert_eq!(json["site_id"], site_id.to_string());
                        assert_eq!(json["parameter_id"], parameter_id.to_string());
                        assert_eq!(json["count"], 42);
                        return;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("frame error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    panic!(
        "did not receive data_ingested event within timeout. accumulated: {accumulated}"
    );
}

#[tokio::test]
#[serial]
async fn sse_receives_multiple_event_types() {
    let (app, token, events, _db) = setup().await;

    let job_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let mut body = response.into_body();

    // Send multiple events in sequence
    let _ = events.send(AppEvent::JobCreated { job_id });
    let _ = events.send(AppEvent::JobProgress {
        job_id,
        status: "running".to_string(),
        progress: Some(5),
        total: Some(10),
    });
    let _ = events.send(AppEvent::JobCompleted {
        job_id,
        status: "completed".to_string(),
        readings_updated: Some(100),
        error_message: None,
    });
    let _ = events.send(AppEvent::DataIngested {
        site_id: None,
        parameter_id: None,
        stream_id,
        count: 7,
    });

    let mut accumulated = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    loop {
        let frame_result = tokio::time::timeout_at(deadline, body.frame()).await;
        match frame_result {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    accumulated.push_str(&String::from_utf8_lossy(data));
                    let frames = parse_sse_frames(&accumulated);
                    let event_types: Vec<&str> =
                        frames.iter().map(|(ev, _)| ev.as_str()).collect();
                    if event_types.contains(&"job_created")
                        && event_types.contains(&"job_progress")
                        && event_types.contains(&"job_completed")
                        && event_types.contains(&"data_ingested")
                    {
                        // Verify job_progress payload
                        let (_, progress) = frames
                            .iter()
                            .find(|(ev, _)| ev == "job_progress")
                            .unwrap();
                        assert_eq!(progress["status"], "running");
                        assert_eq!(progress["progress"], 5);
                        assert_eq!(progress["total"], 10);

                        // Verify job_completed payload
                        let (_, completed) = frames
                            .iter()
                            .find(|(ev, _)| ev == "job_completed")
                            .unwrap();
                        assert_eq!(completed["status"], "completed");
                        assert_eq!(completed["readings_updated"], 100);
                        assert!(completed["error_message"].is_null());

                        return;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("frame error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let frames = parse_sse_frames(&accumulated);
    let event_types: Vec<&str> = frames.iter().map(|(ev, _)| ev.as_str()).collect();
    panic!(
        "did not receive all 4 event types within timeout. got: {event_types:?}\naccumulated: {accumulated}"
    );
}

#[tokio::test]
#[serial]
async fn sse_requires_read_data_permission() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // Token with only read_metadata -- should be rejected
    let token = crate::common::seed_token_read_metadata_only(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        response.status().as_u16(),
        403,
        "read_metadata-only token should not access /api/events"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn sse_rejects_unauthenticated() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "unauthenticated request to /api/events should return 401"
    );

    crate::common::cleanup_test_db(&db).await;
}
