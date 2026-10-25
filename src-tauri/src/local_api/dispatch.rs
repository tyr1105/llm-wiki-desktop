use super::{allowed_import_path, ApiError, ApiState};
use crate::{
    app_state::AppState,
    commands::{
        import_v2_commands as imports, import_v2_file_commands as scans, source_commands,
        task_commands as tasks, workflow_commands as workflows,
    },
    errors::BackendError,
    models::workflow_requests::*,
};
use axum::http::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::Manager;

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "API_REQUEST_INVALID",
            &e.to_string(),
        )
    })
}
fn encode<T: serde::Serialize>(value: T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API_RESPONSE_INVALID",
            "Response could not be serialized.",
        )
    })
}
fn field(value: &Value, key: &str) -> Result<String, ApiError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "API_REQUEST_INVALID",
                &format!("{key} is required."),
            )
        })
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "API_RESOURCE_NOT_FOUND",
        "Unknown API resource.",
    )
}

pub(super) async fn dispatch(
    state: &Arc<ApiState>,
    method: &Method,
    resource: &str,
    query: BTreeMap<String, String>,
    mut body: Value,
) -> Result<(StatusCode, Value), ApiError> {
    let parts = resource.split('/').collect::<Vec<_>>();
    if method == Method::GET && parts == ["health"] {
        return Ok((
            StatusCode::OK,
            json!({"apiVersion":"v1","applicationVersion":env!("CARGO_PKG_VERSION"),"ready":true,"webviewWindowCount":state.app.webview_windows().len()}),
        ));
    }
    if method == Method::GET && parts == ["openapi.json"] {
        return Ok((
            StatusCode::OK,
            serde_json::from_str(include_str!("../../../docs/local-api.openapi.json")).map_err(
                |_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "API_SCHEMA_INVALID",
                        "API schema unavailable.",
                    )
                },
            )?,
        ));
    }
    if method == Method::POST && parts == ["projects", "open"] {
        let _open = state.project_open.lock().await;
        let supplied = PathBuf::from(field(&body, "path")?);
        let root = supplied.canonicalize().map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "API_PROJECT_PATH_INVALID",
                "Project directory is unavailable.",
            )
        })?;
        if !state
            .allowed_projects
            .iter()
            .any(|allowed| allowed.path == root && allowed.validate().is_ok())
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "API_PROJECT_SCOPE_DENIED",
                "Project is outside the startup allowlist.",
            ));
        }
        let existing = state
            .projects
            .lock()
            .map_err(|_| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "API_STATE_LOCKED",
                    "Project binding unavailable.",
                )
            })?
            .iter()
            .find(|(_, path)| Path::new(path).canonicalize().ok().as_ref() == Some(&root))
            .map(|(id, _)| id.clone());
        let frozen = state
            .allowed_projects
            .iter()
            .find(|allowed| allowed.path == root)
            .unwrap()
            .clone();
        let already_open = existing.is_some();
        let app = state.app.clone();
        let (id, root, summary) = state
            .app
            .state::<AppState>()
            .blocking_work
            .clone()
            .run(
                crate::services::BlockingWorkClass::HeavyIo,
                move || -> Result<_, BackendError> {
                    frozen.validate()?;
                    let backend = app.state::<AppState>();
                    if !crate::app_state::ProjectRegistry::is_strict_native_layout(&root)
                        && !root.join(".app/compat/layout.json").is_file()
                    {
                        return Err(BackendError::new(
                            "API_PROJECT_NOT_ENABLED",
                            "Open only an existing native project or an enabled compatible vault.",
                            true,
                            true,
                        ));
                    }
                    let recent = backend.project_service.list_recent_projects()?;
                    let project_id = existing
                        .clone()
                        .or_else(|| backend.project_service.stable_native_project_id(&root))
                        .or_else(|| {
                            recent
                                .iter()
                                .find(|p| {
                                    Path::new(&p.root_path).canonicalize().ok().as_ref()
                                        == Some(&root)
                                })
                                .map(|p| p.project_id.clone())
                        })
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    // Normal project-open policy validates layout and restores compatible grants.
                    let context = if existing.is_some() {
                        backend
                            .resolve_project_context(&project_id, root.to_string_lossy().as_ref())?
                    } else {
                        backend.register_opened_project_authority(project_id.clone(), &root)?
                    };
                    backend.resolve_workflow_read_access(&context)?;
                    frozen.validate()?;
                    let root_string = root.to_string_lossy().into_owned();
                    let summary = app
                        .state::<AppState>()
                        .project_service
                        .scan_project(&context, None);
                    Ok((project_id, root_string, summary))
                },
            )
            .await?;
        if !already_open {
            tasks::set_active_project(
                state.app.clone(),
                tasks::SetActiveProjectRequest {
                    project_id: Some(id.clone()),
                    root_path: Some(root.clone()),
                },
            )
            .await?;
        }
        state
            .projects
            .lock()
            .map_err(|_| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "API_STATE_LOCKED",
                    "Project binding unavailable.",
                )
            })?
            .insert(id.clone(), root.clone());
        return Ok((
            StatusCode::OK,
            json!({"projectId":id,"rootPath":root,"summary":summary}),
        ));
    }
    if parts.first() != Some(&"projects") || parts.len() < 3 {
        return Err(unavailable());
    }
    let project_id = parts[1].to_string();
    let root = state
        .projects
        .lock()
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API_STATE_LOCKED",
                "Project binding unavailable.",
            )
        })?
        .get(&project_id)
        .cloned()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "API_PROJECT_NOT_OPEN",
                "Open the allowed project first.",
            )
        })?;
    let app = state.app.clone();
    // Never accept an asserted ownership root/project from an HTTP body.
    body["projectId"] = json!(project_id);
    body["projectRootPath"] = json!(root);
    if parts.get(2) == Some(&"imports") && parts.len() >= 4 {
        body["sessionId"] = json!(parts[3]);
    }
    if ["workflows", "tasks"].contains(&parts[2]) && parts.len() >= 4 {
        body["taskId"] = json!(parts[3]);
    }
    let check_app = app.clone();
    let check_root = root.clone();
    let allowed = state.allowed_projects.clone();
    app.state::<AppState>()
        .blocking_work
        .clone()
        .run(crate::services::BlockingWorkClass::MetadataIo, move || {
            let matching = allowed
                .iter()
                .find(|r| r.path == Path::new(&check_root))
                .ok_or_else(|| {
                    BackendError::new(
                        "API_PROJECT_SCOPE_DENIED",
                        "Project is outside startup scope.",
                        true,
                        true,
                    )
                })?;
            matching.validate()?;
            let _ = check_app;
            Ok(())
        })
        .await?;
    let result = if parts[2] == "workflows" || (parts[2] == "sources" && parts.len() == 4) {
        match (method.as_str(), parts.as_slice()) {
            ("GET", [_, _, "sources", source_id]) => {
                body["sourceId"] = json!(source_id);
                encode(source_commands::get_source_detail(app.clone(), parse(body)?).await?)?
            }
            ("POST", [_, _, "workflows", "update-wiki"]) => {
                body.as_object_mut().unwrap().remove("taskId");
                body.as_object_mut().unwrap().remove("projectId");
                body.as_object_mut().unwrap().remove("projectRootPath");
                let intent = parse(body)?;
                encode(
                    workflows::start_update_wiki(
                        app.clone(),
                        StartUpdateWikiRequest {
                            project_id,
                            project_root_path: root,
                            intent,
                        },
                    )
                    .await?,
                )?
            }
            ("GET", [_, _, "workflows", _]) => {
                encode(workflows::get_workflow_run(app.clone(), parse(body)?).await?)?
            }
            ("POST", [_, _, "workflows", _, "diff"]) => {
                encode(workflows::get_workflow_file_diff(app.clone(), parse(body)?).await?)?
            }
            ("POST", [_, _, "workflows", _, "confirm"]) => {
                encode(workflows::confirm_workflow_action(app.clone(), parse(body)?).await?)?
            }
            ("POST", [_, _, "workflows", _, "cancel"]) => {
                encode(workflows::cancel_workflow_run(app.clone(), parse(body)?).await?)?
            }
            _ => return Err(unavailable()),
        }
    } else {
        let class = if method == Method::GET || parts[2] == "tasks" {
            crate::services::BlockingWorkClass::MetadataIo
        } else {
            crate::services::BlockingWorkClass::HeavyIo
        };
        let state = state.clone();
        let method = method.to_string();
        let parts = parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        app.state::<AppState>()
            .blocking_work
            .clone()
            .run(class, move || {
                Ok(dispatch_sync(state, method, parts, query, body))
            })
            .await??
    };
    let async_start = method == Method::POST
        && matches!(
            parts.last(),
            Some(&"parse")
                | Some(&"commit")
                | Some(&"paths")
                | Some(&"update-wiki")
                | Some(&"accept")
        );
    Ok((
        if async_start {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        result,
    ))
}

fn dispatch_sync(
    state: Arc<ApiState>,
    method: String,
    parts: Vec<String>,
    query: BTreeMap<String, String>,
    mut body: Value,
) -> Result<Value, ApiError> {
    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
    let app = state.app.clone();
    let project_id = field(&body, "projectId")?;
    let root = field(&body, "projectRootPath")?;
    if parts[2] == "imports" && parts.len() >= 4 && imports::is_temporary_preview_session(parts[3])
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "API_PREVIEW_UNSUPPORTED",
            "API requires a project-backed import session.",
        ));
    }
    if parts[2] == "imports" && method == "POST" {
        let backend = app.state::<AppState>();
        let context = backend.resolve_project_context(&project_id, &root)?;
        backend.require_project_write_access(&context)?;
    }
    Ok(match (method.as_str(), parts.as_slice()) {
        ("GET", [_, _, "sources"]) => {
            let offset = query
                .get("offset")
                .map(|v| v.parse::<usize>())
                .transpose()
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "API_QUERY_INVALID",
                        "offset must be an integer.",
                    )
                })?
                .unwrap_or(0);
            let backend = app.state::<AppState>();
            let context = backend.resolve_project_context(&project_id, &root)?;
            encode(backend.workflow_service.list_update_wiki_sources(
                &context,
                query.get("q").map(String::as_str).unwrap_or(""),
                offset,
            )?)?
        }
        ("POST", [_, _, "imports"]) => {
            if body.get("resourceMode").is_none() {
                body["resourceMode"] = json!("balanced");
            }
            let request: imports::CreateImportSessionV2Request = parse(body)?;
            let backend = app.state::<AppState>();
            encode(backend.with_current_project_write_access(
                &request.project_id,
                &request.project_root_path,
                |permit, _context| {
                    backend.import_v2_service.create_session_authorized(
                        permit,
                        &backend.file_store,
                        request.resource_mode,
                    )
                },
            )?)?
        }
        ("GET", [_, _, "imports", _]) => {
            body["historyBatchId"] = Value::Null;
            encode(imports::get_import_session_v2(app.state(), parse(body)?)?)?
        }
        ("GET", [_, _, "imports", _, "overview"]) => {
            body["historyBatchId"] = Value::Null;
            encode(imports::get_import_session_overview_v2(app.state(), parse(body)?)?)?
        }
        ("POST", [_, _, "imports", _, "text"]) => {
            encode(imports::add_import_text_v2(app.state(), parse(body)?)?)?
        }
        ("POST", [_, _, "imports", _, "paths"]) => {
            let paths = body
                .get("sourcePaths")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "API_REQUEST_INVALID",
                        "sourcePaths is required.",
                    )
                })?;
            if paths.is_empty() || paths.len() > 100 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "API_PATH_COUNT_INVALID",
                    "Supply 1 to 100 paths.",
                ));
            }
            let mut canonical = Vec::new();
            for path in paths {
                canonical.push(
                    allowed_import_path(
                        Path::new(path.as_str().ok_or_else(|| {
                            ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "API_REQUEST_INVALID",
                                "Each source path must be a string.",
                            )
                        })?),
                        &state.allowed_imports,
                    )?
                    .to_string_lossy()
                    .into_owned(),
                );
            }
            body["sourcePaths"] = json!(canonical);
            let allowed = state.allowed_imports.clone();
            let guard: scans::DiscoveryPathGuard = Arc::new(move |path| {
                super::scope::validate_import_path(path, &allowed).map(|_| ())
            });
            encode(scans::start_add_import_paths_with_guard(
                app.clone(),
                parse(body)?,
                Some(guard),
            )?)?
        }
        ("POST", [_, _, "imports", _, "inputs"]) => {
            let mut request: imports::AddImportItemsV2Request = parse(body)?;
            if request.inputs.is_empty() || request.inputs.len() > 100 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "API_INPUT_COUNT_INVALID",
                    "Supply 1 to 100 URL inputs.",
                ));
            }
            for input in &mut request.inputs {
                let url = url::Url::parse(&input.locator).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "API_URL_INVALID",
                        "Supply an HTTP or HTTPS URL.",
                    )
                })?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "API_URL_INVALID",
                        "Supply an HTTP or HTTPS URL.",
                    ));
                }
                input.normalized_locator = None;
                input.source_identity = None;
            }
            if request
                .inputs
                .iter()
                .any(|i| i.kind != crate::models::import_v2::ImportInputKind::Url)
            {
                return Err(ApiError::new(StatusCode::BAD_REQUEST,"API_INPUT_KIND_INVALID","Use /paths for local discovery or /text for supplied text. /inputs accepts URL inputs only."));
            }
            encode(imports::add_import_items_v2(app.state(), request)?)?
        }
        ("GET", [_, _, "imports", _, "scans", task_id]) => {
            body["taskId"] = json!(task_id);
            let scan = scans::get_import_scan_result_v2(app.state(), parse(body)?)?;
            for file in &scan.files {
                allowed_import_path(
                    Path::new(&file.source_identity.canonical_path),
                    &state.allowed_imports,
                )?;
            }
            encode(scan)?
        }
        ("POST", [_, _, "imports", _, "scans", task_id, "accept"]) => {
            body["taskId"] = json!(task_id);
            let scan = scans::get_import_scan_result_v2(
                app.state(),
                scans::GetImportScanResultV2Request {
                    project_id: project_id.clone(),
                    project_root_path: root.clone(),
                    session_id: parts[3].into(),
                    task_id: (*task_id).into(),
                },
            )?;
            for file in &scan.files {
                allowed_import_path(
                    Path::new(&file.source_identity.canonical_path),
                    &state.allowed_imports,
                )?;
            }
            encode(scans::accept_import_scan_with_guard(
                app.clone(),
                app.state(),
                parse(body)?,
                Some(super::scope::input_guard(state.allowed_imports.clone())),
            )?)?
        }
        ("POST", [_, _, "imports", _, "parse"]) => {
            let request: imports::StartImportBatchV2Request = parse(body)?;
            let backend = app.state::<AppState>();
            let context = backend.resolve_project_context(&project_id, &root)?;
            let session = backend.import_v2_service.load_session(
                &context,
                &backend.file_store,
                &request.session_id,
            )?;
            let guard = super::scope::input_guard(state.allowed_imports.clone());
            for id in &request.item_ids {
                let item = session
                    .items
                    .iter()
                    .find(|i| &i.item_id == id)
                    .ok_or_else(|| {
                        ApiError::new(
                            StatusCode::NOT_FOUND,
                            "API_IMPORT_ITEM_NOT_FOUND",
                            "Selected import item does not exist.",
                        )
                    })?;
                guard(&item.input)?;
            }
            encode(imports::start_import_batch_with_guard(
                app.clone(),
                app.state(),
                request,
                Some(guard),
            )?)?
        }
        ("POST", [_, _, "imports", _, "commit"]) => encode(imports::confirm_import_session_v2(
            app.clone(),
            app.state(),
            parse(body)?,
        )?)?,
        ("GET", [_, _, "tasks", _]) => encode(tasks::get_task(app.state(), parse(body)?)?)?,
        ("POST", [_, _, "tasks", _, "cancel"]) => {
            encode(tasks::cancel_task(app.state(), parse(body)?)?)?
        }
        _ => return Err(unavailable()),
    })
}
