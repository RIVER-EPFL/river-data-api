use super::{Deployment, served_cors_origins};

fn origins(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn test_served_cors_origins_keeps_loopback_locally() {
    let list = origins(&["http://localhost:5173", "http://127.0.0.1:3005"]);
    let (kept, dropped) = served_cors_origins(Deployment::Local, &list);
    assert_eq!(kept, list, "a local API is reached from a local dev server");
    assert!(dropped.is_empty());
}

#[test]
fn test_served_cors_origins_drops_loopback_when_deployed() {
    let list = origins(&[
        "http://localhost:5173",
        "http://127.0.0.1:3005",
        "http://[::1]:5173",
        "https://river-data.epfl.ch",
    ]);
    for deployment in [Deployment::Dev, Deployment::Stage, Deployment::Prod] {
        let (kept, dropped) = served_cors_origins(deployment, &list);
        assert_eq!(kept, origins(&["https://river-data.epfl.ch"]));
        assert_eq!(
            dropped.len(),
            3,
            "every loopback origin is named, not served"
        );
    }
}

#[test]
fn test_served_cors_origins_leaves_the_deployment_with_none() {
    // Dropping every origin is not the same as allowing every origin: the caller decides what
    // an empty allowlist means, and must not read this as "unset".
    let (kept, dropped) =
        served_cors_origins(Deployment::Prod, &origins(&["http://localhost:5173"]));
    assert!(kept.is_empty());
    assert_eq!(dropped.len(), 1);
}
