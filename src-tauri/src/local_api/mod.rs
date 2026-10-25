//! Opt-in loopback REST transport over the existing desktop command boundary.
mod config;
mod dispatch;
mod retry;
mod scope;
mod security;
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
pub use config::{handle_token_command, ApiLaunch};
pub use security::InstanceLease;
use security::{allowed_import_path, authorize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::AppHandle;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    error: crate::errors::BackendError,
}
impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        Self {
            status,
            error: crate::errors::BackendError::new(code, message, true, false),
        }
    }
}
impl From<crate::errors::BackendError> for ApiError {
    fn from(error: crate::errors::BackendError) -> Self {
        let code = &error.code;
        let status = if code.contains("UNTRUST")
            || code.contains("READ_ONLY")
            || code.contains("RESTRICTED")
            || code.contains("REQUIRES_TRUST")
            || code.contains("SCOPE_DENIED")
            || code.contains("PREVIEW_UNSUPPORTED")
        {
            StatusCode::FORBIDDEN
        } else if code.contains("NOT_FOUND") {
            StatusCode::NOT_FOUND
        } else if code.contains("STALE")
            || code.contains("CHANGED")
            || code.contains("MISMATCH")
            || code.contains("CONFIRMATION")
            || code.contains("LOCKED")
        {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        Self { status, error }
    }
}
struct ApiState {
    app: AppHandle,
    port: u16,
    token: String,
    allowed_projects: Vec<scope::FrozenRoot>,
    allowed_imports: Vec<scope::FrozenRoot>,
    projects: Mutex<HashMap<String, String>>,
    project_open: tokio::sync::Mutex<()>,
    writes: Arc<retry::RetryBroker>,
    operations: Arc<tokio::sync::Semaphore>,
    requests: Arc<tokio::sync::Semaphore>,
}

pub fn start(app: AppHandle, launch: ApiLaunch) {
    let state = Arc::new(ApiState {
        app,
        port: launch.port,
        token: launch.token,
        allowed_projects: launch.projects,
        allowed_imports: launch.imports,
        projects: Mutex::new(HashMap::new()),
        project_open: tokio::sync::Mutex::new(()),
        writes: Arc::new(retry::RetryBroker::default()),
        operations: Arc::new(tokio::sync::Semaphore::new(8)),
        requests: Arc::new(tokio::sync::Semaphore::new(24)),
    });
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(launch.listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Local API listener failed: {e}");
                return;
            }
        };
        let router = Router::new()
            .route("/api/v1/{*resource}", any(handle))
            .with_state(state);
        if let Err(e) = axum::serve(listener, router).await {
            eprintln!("Local API stopped: {e}");
        }
    });
}

fn response(status: StatusCode, body: Value, request_id: &str) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert("x-request-id", request_id.parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            "www-authenticate",
            "Bearer realm=\"llm-wiki-local\"".parse().unwrap(),
        );
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        response
            .headers_mut()
            .insert("allow", "GET, POST".parse().unwrap());
    }
    response
}
async fn handle(State(state): State<Arc<ApiState>>, request: Request) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let failed = |e: ApiError| {
        response(
            e.status,
            json!({"error":e.error,"requestId":request_id}),
            &request_id,
        )
    };
    if let Err(e) = authorize(request.headers(), &state.token, state.port) {
        return failed(e);
    }
    let _permit = match state.requests.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return failed(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "API_BUSY",
                "Too many concurrent requests.",
            ))
        }
    };
    let method = request.method().clone();
    let resource = request
        .uri()
        .path()
        .strip_prefix("/api/v1/")
        .unwrap_or("")
        .to_string();
    let query = url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    if method != Method::GET && method != Method::POST {
        return failed(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "API_METHOD_NOT_ALLOWED",
            "Use the documented method.",
        ));
    }
    let key = if method == Method::POST {
        match request
            .headers()
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .filter(|v| {
                !v.is_empty()
                    && v.len() <= 128
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            }) {
            Some(k) => Some(k.to_string()),
            None => return failed(ApiError::new(
                StatusCode::BAD_REQUEST,
                "API_IDEMPOTENCY_KEY_REQUIRED",
                "Supply an Idempotency-Key of up to 128 letters, digits, hyphens or underscores.",
            )),
        }
    } else {
        None
    };
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        to_bytes(request.into_body(), 2 * 1024 * 1024),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Err(_) => {
            return failed(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "API_BODY_TIMEOUT",
                "Request body did not arrive within 15 seconds.",
            ))
        }
        Ok(Err(_)) => {
            return failed(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "API_BODY_TOO_LARGE",
                "Request body exceeds 2 MiB.",
            ))
        }
    };
    let body = if bytes.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) if v.is_object() => v,
            _ => {
                return failed(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "API_JSON_INVALID",
                    "Supply a JSON object.",
                ))
            }
        }
    };
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(format!("{method} {resource} {query:?} {body}").as_bytes())
    );
    let execute = {
        let state = state.clone();
        let request_id = request_id.clone();
        move || async move {
            let _operation = match state.operations.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    return retry::Cached {
                        status: StatusCode::TOO_MANY_REQUESTS,
                        body: json!({"error":{"code":"API_OPERATION_BUSY","message":"Too many active API operations."},"requestId":request_id}),
                    }
                }
            };
            match dispatch::dispatch(&state, &method, &resource, query, body).await {
                Ok((status, data)) => retry::Cached {
                    status,
                    body: json!({"data":data,"requestId":request_id}),
                },
                Err(e) => retry::Cached {
                    status: e.status,
                    body: json!({"error":e.error,"requestId":request_id}),
                },
            }
        }
    };
    let mut result = if let Some(key) = key {
        let receiver = match state.writes.submit(key, fingerprint, execute).await {
            Ok(receiver) => receiver,
            Err(e) => return failed(e),
        };
        match retry::wait(receiver).await {
            Ok(result) => result,
            Err(e) => return failed(e),
        }
    } else {
        execute().await
    };
    result.body["requestId"] = json!(request_id);
    response(result.status, result.body, &request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("127.0.0.1:34343"));
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }
    #[test]
    fn browser_and_rebinding_requests_never_pass_authentication() {
        let token = "a".repeat(64);
        let mut h = headers(&token);
        assert!(authorize(&h, &token, 34343).is_ok());
        h.insert("origin", HeaderValue::from_static("null"));
        assert_eq!(
            authorize(&h, &token, 34343).unwrap_err().status,
            StatusCode::FORBIDDEN
        );
        h.remove("origin");
        h.insert("host", HeaderValue::from_static("attacker.test:34343"));
        assert_eq!(
            authorize(&h, &token, 34343).unwrap_err().status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            authorize(&headers("wrong"), &token, 34343)
                .unwrap_err()
                .status,
            StatusCode::UNAUTHORIZED
        );
        let mut absent = headers(&token);
        absent.remove("authorization");
        assert_eq!(
            authorize(&absent, &token, 34343).unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
    }
    #[test]
    fn instance_lease_excludes_a_second_backend_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLease::acquire(dir.path()).unwrap();
        assert!(InstanceLease::acquire(dir.path()).is_err());
        drop(lock);
        assert!(InstanceLease::acquire(dir.path()).is_ok());
    }
    #[test]
    fn allowlist_rejects_outside_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = root.path().join("source.md");
        std::fs::write(&file, "source").unwrap();
        let other = outside.path().join("secret.md");
        std::fs::write(&other, "secret").unwrap();
        assert!(
            allowed_import_path(&file, &[scope::FrozenRoot::capture(root.path()).unwrap()]).is_ok()
        );
        assert!(
            allowed_import_path(&other, &[scope::FrozenRoot::capture(root.path()).unwrap()])
                .is_err()
        );
        #[cfg(unix)]
        {
            let link = root.path().join("link.md");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert!(allowed_import_path(
                &link,
                &[scope::FrozenRoot::capture(root.path()).unwrap()]
            )
            .is_err());
        }
    }
    #[test]
    fn child_instance_lease_probe() {
        if let Some(root) = std::env::var_os("LLM_WIKI_TEST_INSTANCE_ROOT") {
            let acquired = InstanceLease::acquire(std::path::Path::new(&root));
            assert_eq!(
                acquired.is_ok(),
                std::env::var("LLM_WIKI_TEST_INSTANCE_EXPECT").unwrap() == "available"
            );
        }
    }
    #[test]
    fn instance_lease_excludes_another_process_before_backend_initialization() {
        let root = tempfile::tempdir().unwrap();
        let lease = InstanceLease::acquire(root.path()).unwrap();
        let probe = |expected: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "local_api::tests::child_instance_lease_probe",
                    "--nocapture",
                ])
                .env("LLM_WIKI_TEST_INSTANCE_ROOT", root.path())
                .env("LLM_WIKI_TEST_INSTANCE_EXPECT", expected)
                .status()
                .unwrap()
        };
        assert!(probe("busy").success());
        drop(lease);
        assert!(probe("available").success());
    }
    #[cfg(unix)]
    #[test]
    fn startup_scope_cannot_be_redirected_to_a_private_directory() {
        let parent = tempfile::tempdir().unwrap();
        let incoming = parent.path().join("incoming");
        let private = parent.path().join("private");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("secret.md"), "private").unwrap();
        let roots = vec![scope::FrozenRoot::capture(&incoming).unwrap()];
        std::fs::rename(&incoming, parent.path().join("old-incoming")).unwrap();
        std::os::unix::fs::symlink(&private, &incoming).unwrap();
        assert!(allowed_import_path(&incoming.join("secret.md"), &roots).is_err());
    }
}
