//! The LSP layer: transport, lifecycle, and request routing.
//!
//! This is the only crate that knows LSP exists. Everything below it speaks
//! [`javelin_protocol`] and plain Rust types, which is what keeps the Java engine
//! usable from a CLI, a test harness, or a future non-LSP frontend.
//!
//! The loop itself stays single-threaded, and requests are still answered on
//! it: they read cached state and are bounded by the answer, not by the file.
//! What has moved off it is reparsing, which is bounded by the file — see the
//! `pool` module. The loop selects between the client and the pool, so a 300 ms
//! reparse of a generated god-class is 300 ms of a worker's time and none of
//! the editor's.

pub mod cancellation;
pub mod capabilities;
pub mod classpath;
mod dependencies;
mod diagnostics;
pub mod handlers;
pub mod hierarchy;
mod jdk_sources;
pub mod member_refs;
mod modules;
mod pool;
mod progress;
pub mod state;
pub mod workspace;

use javelin_core::{CancellationToken, Cancelled, Config, Error, Result};
use javelin_document::DocumentId;
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare, Completion,
    DocumentHighlightRequest, DocumentSymbolRequest, FoldingRangeRequest, GotoDefinition,
    GotoImplementation, GotoTypeDefinition, HoverRequest, InlayHintRequest, References,
    Request as _, SelectionRangeRequest, SemanticTokensFullRequest, SemanticTokensRangeRequest,
    TypeHierarchyPrepare, TypeHierarchySubtypes, TypeHierarchySupertypes, WorkspaceSymbolRequest,
};

pub use capabilities::{capabilities_json, server_capabilities};
pub use state::ServerState;

use cancellation::InFlight;

/// Serve LSP over stdin/stdout until the client disconnects.
///
/// Startup order matters: the connection is established and `initialize` is
/// answered before any Java work happens, so the editor sees a live server in
/// milliseconds regardless of how large the workspace is.
pub fn run_stdio(config: Config) -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let result = serve(&connection, config);
    // The writer thread only finishes once every sender is gone, so the
    // connection must be dropped *before* joining. Holding it across the join
    // deadlocks the process on exit — the editor's "server did not shut down"
    // dialog.
    drop(connection);
    io_threads
        .join()
        .map_err(|err| Error::internal(format!("i/o threads: {err}")))?;
    result
}

/// Serve one already-established connection.
///
/// Split out from [`run_stdio`] so integration tests can drive a real handshake
/// over an in-memory connection instead of spawning a process and framing JSON.
pub fn serve(connection: &Connection, config: Config) -> Result<()> {
    let capabilities = capabilities_json()?;

    let init_params = match connection.initialize(capabilities) {
        Ok(params) => params,
        // An editor that crashes mid-handshake is not a server error. Exit
        // quietly rather than with a backtrace the user cannot act on.
        Err(err) if err.channel_is_disconnected() => {
            tracing::info!("client disconnected during initialize");
            return Ok(());
        }
        Err(err) => return Err(Error::internal(format!("initialize failed: {err}"))),
    };

    let config = merge_client_settings(config, &init_params);
    let mut state = ServerState::new(config)?;
    if init_params
        .pointer("/capabilities/workspace/inlayHint/refreshSupport")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        state.refreshes_hints();
    }
    let mut in_flight = InFlight::new();
    // Cloned out of the state up front so the loop can wait on it and still
    // hand `install` a `&mut` to the state it writes into.
    let parses = state.parse_results();
    let classpath = state.classpath_results();
    let workspace = state.workspace_results();
    let diagnostics = state.diagnostics_results();
    let progress = state.progress_results();
    let show_progress = progress::supported(&init_params);
    tracing::info!("initialized");
    // Here and nowhere earlier. `Connection::initialize` sends the
    // `initialize` response and then waits for the client's `initialized`
    // notification, so reaching this line means both have happened: the editor
    // has its capabilities, and the handshake this job is allowed to follow is
    // over. Nothing above it looks at a filesystem for a JDK. The notification
    // itself never reaches `handle_notification` — `lsp-server` consumes it —
    // which is why this is not an arm there.
    let roots = workspace::roots(&init_params);
    state.start_classpath(roots.clone());
    state.start_workspace(roots);
    // Its answer arrives as a `Response` the loop logs and drops: a watcher
    // that could not be registered costs changes made outside the editor,
    // nothing else.
    if let Some(registration) = watcher_registration(&init_params) {
        if connection
            .sender
            .send(Message::Request(registration))
            .is_err()
        {
            return Ok(());
        }
        state.watching_files();
    }

    loop {
        crossbeam_channel::select! {
            recv(connection.receiver) -> message => match message {
                Ok(message) => match handle_message(connection, &mut state, &mut in_flight, message)
                {
                    Flow::Continue => {}
                    Flow::Stop => return Ok(()),
                },
                // The client hung up without saying goodbye.
                Err(_) => return Ok(()),
            },
            // A finished reparse. Installing is a map insert and a version
            // compare; the expensive part already happened on a worker.
            recv(parses) -> outcome => match outcome {
                Ok(outcome) => {
                    state.install(outcome);
                    // A hint request it made the client wait for.
                    if send_hint_refresh(connection, &mut state) == Flow::Stop {
                        return Ok(());
                    }
                }
                // Unreachable: the pool holds a sender of its own so that a
                // worker dying cannot turn this arm into a spin.
                Err(_) => return Ok(()),
            },
            // The JDK, read once in the background. Installing is a field
            // write; the eleven seconds were spent on another thread.
            recv(classpath) -> arrived => match arrived {
                Ok(classpath) => state.install_classpath(classpath),
                // Unreachable for the same reason: the state keeps a sender.
                Err(_) => return Ok(()),
            },
            // The workspace's own sources, walked once in the background.
            // Installing is a field write and a log line.
            recv(workspace) -> arrived => match arrived {
                Ok(workspace) => state.install_workspace(workspace),
                Err(_) => return Ok(()),
            },
            // A judgement the diagnostics worker finished. Sent only if the
            // editor still shows the version judged; a newer one is coming.
            recv(diagnostics) -> judged => match judged {
                Ok(published) => {
                    if state.is_current(&published)
                        && connection
                            .sender
                            .send(Message::Notification(diagnostics::notification(&published)))
                            .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(_) => return Ok(()),
            },
            // The workers' background progress, for a client that shows it.
            recv(progress) -> event => match event {
                Ok(event) => {
                    if show_progress
                        && progress::messages(&event)
                            .into_iter()
                            .any(|message| connection.sender.send(message).is_err())
                    {
                        return Ok(());
                    }
                }
                Err(_) => return Ok(()),
            },
        }
    }
}

/// LSP's `FileChangeType` as its wire number: `1` created, `2` changed, `3`
/// deleted.
fn file_change_kind(kind: lsp_types::FileChangeType) -> u32 {
    match kind {
        lsp_types::FileChangeType::CREATED => 1,
        lsp_types::FileChangeType::DELETED => 3,
        _ => 2,
    }
}

/// The ids of the requests this server sends, so their answers can be told
/// from anything unsolicited. One id for every hint refresh: the answer
/// carries nothing to tell them apart by.
const WATCH_REGISTRATION: &str = "javelin/watch-java-sources";
const HINT_REFRESH: &str = "javelin/inlay-hint-refresh";

/// A request asking the client to report every `.java` file created, changed
/// or deleted in the workspace, and every other path created or deleted — a
/// folder, which takes files with it that no event names — or `None` for a
/// client that did not say it can register a watcher, which must not be sent
/// one.
fn watcher_registration(init_params: &serde_json::Value) -> Option<Request> {
    let dynamic = init_params
        .pointer("/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    dynamic.then(|| Request {
        id: RequestId::from(WATCH_REGISTRATION.to_owned()),
        method: "client/registerCapability".to_owned(),
        params: serde_json::json!({
            "registrations": [{
                "id": "javelin-java-sources",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": [
                    { "globPattern": "**/*.java" },
                    // Every change to what the classpath's answer is keyed
                    // by, which `javelin_build::is_build_path` names.
                    {
                        "globPattern": "**/{pom.xml,*.gradle,*.gradle.kts,*.versions.toml,\
                                        gradle.properties,gradle-wrapper.properties,\
                                        maven-wrapper.properties}"
                    },
                    { "globPattern": "**/{.mvn,buildSrc,build-logic}/**" },
                    // What Lombok generates, and what it calls it.
                    { "globPattern": "**/lombok.config" },
                    // Create and delete only (WatchKind 1 | 4): a folder
                    // created or deleted, not every file changed.
                    { "globPattern": "**/*", "kind": 5 }
                ] }
            }]
        }),
    })
}

/// Whether the loop should keep going after a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

fn handle_message(
    connection: &Connection,
    state: &mut ServerState,
    in_flight: &mut InFlight,
    message: Message,
) -> Flow {
    match message {
        Message::Request(request) => {
            // `handle_shutdown` answers the request and then waits for the
            // `exit` notification, so a `true` or an error both mean the
            // session is over and the request must not be routed on.
            match connection.handle_shutdown(&request) {
                Ok(true) => {
                    in_flight.cancel_all();
                    tracing::info!("shutdown requested");
                    return Flow::Stop;
                }
                Err(err) => {
                    in_flight.cancel_all();
                    tracing::warn!(%err, "unclean shutdown");
                    return Flow::Stop;
                }
                Ok(false) => {}
            }
            let response = handle_request(state, in_flight, request);
            if connection.sender.send(Message::Response(response)).is_err() {
                tracing::warn!("client disconnected while responding");
                return Flow::Stop;
            }
            // After the answer it is about: the client asks again for what
            // it shows, and the memo answers what this request worked out.
            send_hint_refresh(connection, state)
        }
        Message::Notification(notification) => {
            handle_notification(state, in_flight, notification);
            Flow::Continue
        }
        // A client that refuses the watcher registration costs changes made
        // outside the editor, which is worth a warning; a hint refresh's
        // answer, either way, changes nothing.
        Message::Response(response) => {
            if response.id == RequestId::from(WATCH_REGISTRATION.to_owned()) {
                if let Err(error) = &response.response_result {
                    tracing::warn!(
                        message = %error.message,
                        "the client refused the file watcher; changes on disk will not reach the index"
                    );
                    state.watcher_refused();
                }
            } else if response.id == RequestId::from(HINT_REFRESH.to_owned()) {
                tracing::debug!(error = ?response.response_result.err(), "hint refresh answered");
                state.hint_refresh_answered();
                return send_hint_refresh(connection, state);
            } else {
                tracing::debug!(id = ?response.id, "ignoring unsolicited response");
            }
            Flow::Continue
        }
    }
}

/// Send the client a `workspace/inlayHint/refresh` if the state owes one.
fn send_hint_refresh(connection: &Connection, state: &mut ServerState) -> Flow {
    let refresh = Request {
        id: RequestId::from(HINT_REFRESH.to_owned()),
        method: "workspace/inlayHint/refresh".to_owned(),
        params: serde_json::Value::Null,
    };
    if state.take_hint_refresh() && connection.sender.send(Message::Request(refresh)).is_err() {
        return Flow::Stop;
    }
    Flow::Continue
}

/// Apply the `initializationOptions` blob the client sent, if any.
fn merge_client_settings(default: Config, init_params: &serde_json::Value) -> Config {
    let Some(options) = init_params.get("initializationOptions") else {
        return default;
    };
    let (config, warning) = Config::from_json(options);
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    config
}

fn handle_request(state: &mut ServerState, in_flight: &mut InFlight, request: Request) -> Response {
    let id = request.id.clone();
    let token = in_flight.begin(id.clone());
    let response = route(state, &token, request);
    // On every path out, including the cancelled and malformed ones: the map is
    // the record of what is still running, and an entry nobody removes stays.
    in_flight.finish(&id);
    response
}

/// Dispatch one request to its handler.
///
/// Separate from [`handle_request`] so a test can supply a token that is already
/// cancelled, which the single-threaded loop can otherwise never produce.
fn route(state: &mut ServerState, token: &CancellationToken, request: Request) -> Response {
    match request.method.as_str() {
        DocumentSymbolRequest::METHOD => match extract_params::<DocumentSymbolRequest>(request) {
            Ok((id, params)) => reply(id, handlers::document_symbol::handle(state, &params, token)),
            Err(response) => response,
        },
        FoldingRangeRequest::METHOD => match extract_params::<FoldingRangeRequest>(request) {
            Ok((id, params)) => reply(id, handlers::folding_range::handle(state, &params, token)),
            Err(response) => response,
        },
        InlayHintRequest::METHOD => match extract_params::<InlayHintRequest>(request) {
            Ok((id, params)) => reply(id, handlers::inlay_hint::handle(state, &params, token)),
            Err(response) => response,
        },
        SelectionRangeRequest::METHOD => match extract_params::<SelectionRangeRequest>(request) {
            Ok((id, params)) => reply(id, handlers::selection_range::handle(state, &params, token)),
            Err(response) => response,
        },
        DocumentHighlightRequest::METHOD => {
            match extract_params::<DocumentHighlightRequest>(request) {
                Ok((id, params)) => reply(
                    id,
                    handlers::document_highlight::handle(state, &params, token),
                ),
                Err(response) => response,
            }
        }
        SemanticTokensFullRequest::METHOD => {
            match extract_params::<SemanticTokensFullRequest>(request) {
                Ok((id, params)) => {
                    reply(id, handlers::semantic_tokens::handle(state, &params, token))
                }
                Err(response) => response,
            }
        }
        SemanticTokensRangeRequest::METHOD => {
            match extract_params::<SemanticTokensRangeRequest>(request) {
                Ok((id, params)) => reply(
                    id,
                    handlers::semantic_tokens::handle_range(state, &params, token),
                ),
                Err(response) => response,
            }
        }
        GotoDefinition::METHOD => match extract_params::<GotoDefinition>(request) {
            Ok((id, params)) => reply(id, handlers::definition::handle(state, &params, token)),
            Err(response) => response,
        },
        GotoTypeDefinition::METHOD => match extract_params::<GotoTypeDefinition>(request) {
            Ok((id, params)) => reply(id, handlers::type_definition::handle(state, &params, token)),
            Err(response) => response,
        },
        References::METHOD => match extract_params::<References>(request) {
            Ok((id, params)) => reply(id, handlers::references::handle(state, &params, token)),
            Err(response) => response,
        },
        HoverRequest::METHOD => match extract_params::<HoverRequest>(request) {
            Ok((id, params)) => reply(id, handlers::hover::handle(state, &params, token)),
            Err(response) => response,
        },
        Completion::METHOD => match extract_params::<Completion>(request) {
            Ok((id, params)) => reply(id, handlers::completion::handle(state, &params, token)),
            Err(response) => response,
        },
        WorkspaceSymbolRequest::METHOD => match extract_params::<WorkspaceSymbolRequest>(request) {
            Ok((id, params)) => reply(
                id,
                handlers::workspace_symbol::handle(state, &params, token),
            ),
            Err(response) => response,
        },
        GotoImplementation::METHOD => match extract_params::<GotoImplementation>(request) {
            Ok((id, params)) => reply(
                id,
                handlers::hierarchy::implementation(state, &params, token),
            ),
            Err(response) => response,
        },
        TypeHierarchyPrepare::METHOD => match extract_params::<TypeHierarchyPrepare>(request) {
            Ok((id, params)) => reply(
                id,
                handlers::hierarchy::prepare_type_hierarchy(state, &params, token),
            ),
            Err(response) => response,
        },
        TypeHierarchySupertypes::METHOD => {
            match extract_params::<TypeHierarchySupertypes>(request) {
                Ok((id, params)) => {
                    reply(id, handlers::hierarchy::supertypes(state, &params, token))
                }
                Err(response) => response,
            }
        }
        TypeHierarchySubtypes::METHOD => match extract_params::<TypeHierarchySubtypes>(request) {
            Ok((id, params)) => reply(id, handlers::hierarchy::subtypes(state, &params, token)),
            Err(response) => response,
        },
        CallHierarchyPrepare::METHOD => match extract_params::<CallHierarchyPrepare>(request) {
            Ok((id, params)) => reply(
                id,
                handlers::hierarchy::prepare_call_hierarchy(state, &params, token),
            ),
            Err(response) => response,
        },
        CallHierarchyIncomingCalls::METHOD => {
            match extract_params::<CallHierarchyIncomingCalls>(request) {
                Ok((id, params)) => reply(
                    id,
                    handlers::hierarchy::incoming_calls(state, &params, token),
                ),
                Err(response) => response,
            }
        }
        CallHierarchyOutgoingCalls::METHOD => {
            match extract_params::<CallHierarchyOutgoingCalls>(request) {
                Ok((id, params)) => reply(
                    id,
                    handlers::hierarchy::outgoing_calls(state, &params, token),
                ),
                Err(response) => response,
            }
        }
        // Answering is what matters: a client that gets no reply waits forever,
        // and MethodNotFound is the honest answer for a feature that is not here.
        _ => Response::new_err(
            request.id,
            lsp_server::ErrorCode::MethodNotFound as i32,
            format!("{} is not implemented yet", request.method),
        ),
    }
}

/// Deserialize a request's params, or the error response to send instead.
fn extract_params<R>(request: Request) -> std::result::Result<(RequestId, R::Params), Response>
where
    R: lsp_types::request::Request,
{
    let id = request.id.clone();
    match request.extract::<R::Params>(R::METHOD) {
        Ok(extracted) => Ok(extracted),
        Err(ExtractError::JsonError { method, error }) => {
            tracing::warn!(method, %error, "malformed request params");
            Err(Response::new_err(
                id,
                lsp_server::ErrorCode::InvalidParams as i32,
                format!("{} params are not valid: {error}", R::METHOD),
            ))
        }
        // Unreachable: the method was matched to pick this handler.
        Err(ExtractError::MethodMismatch(request)) => Err(Response::new_err(
            id,
            lsp_server::ErrorCode::InvalidRequest as i32,
            format!("{} was routed as {}", request.method, R::METHOD),
        )),
    }
}

/// Turn a handler's answer into a response.
///
/// Serialising by hand rather than through `Response::new_ok`, which unwraps: a
/// handler that produced something unserialisable is a bug in Javelin, and the
/// client should hear about it as an error rather than as a dead server.
fn reply<T: serde::Serialize>(
    id: RequestId,
    answer: std::result::Result<T, Cancelled>,
) -> Response {
    match answer.map(|value| serde_json::to_value(value)) {
        Ok(Ok(value)) => Response {
            id,
            response_result: Ok(value),
        },
        Ok(Err(err)) => Response::new_err(
            id,
            lsp_server::ErrorCode::InternalError as i32,
            format!("the answer could not be serialised: {err}"),
        ),
        Err(Cancelled) => Response::new_err(
            id,
            lsp_server::ErrorCode::RequestCanceled as i32,
            "request cancelled".to_owned(),
        ),
    }
}

fn handle_notification(
    state: &mut ServerState,
    in_flight: &mut InFlight,
    notification: Notification,
) {
    use lsp_types::notification::{
        Cancel, DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument,
        DidOpenTextDocument, DidSaveTextDocument,
    };

    match notification.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Some(params) = cast::<DidOpenTextDocument>(notification) {
                let document = params.text_document;
                state.did_open(
                    DocumentId::new(document.uri.as_str()),
                    document.version,
                    document.text,
                );
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Some(params) = cast::<DidChangeTextDocument>(notification) {
                let changes: Vec<_> = params
                    .content_changes
                    .iter()
                    .map(state::to_change)
                    .collect();
                state.did_change(
                    &DocumentId::new(params.text_document.uri.as_str()),
                    params.text_document.version,
                    &changes,
                );
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Some(params) = cast::<DidCloseTextDocument>(notification) {
                state.did_close(&DocumentId::new(params.text_document.uri.as_str()));
            }
        }
        DidSaveTextDocument::METHOD => {
            if let Some(params) = cast::<DidSaveTextDocument>(notification) {
                state.did_save(&DocumentId::new(params.text_document.uri.as_str()));
            }
        }
        DidChangeWatchedFiles::METHOD => {
            if let Some(params) = cast::<DidChangeWatchedFiles>(notification) {
                let uris: Vec<(String, u32)> = params
                    .changes
                    .iter()
                    .map(|change| (change.uri.as_str().to_owned(), file_change_kind(change.typ)))
                    .collect();
                state.did_change_watched(&uris);
            }
        }
        Cancel::METHOD => {
            if let Some(params) = cast::<Cancel>(notification) {
                in_flight.cancel(&to_request_id(params.id));
            }
        }
        other => tracing::trace!(method = other, "ignoring notification"),
    }
}

fn to_request_id(id: lsp_types::NumberOrString) -> RequestId {
    match id {
        lsp_types::NumberOrString::Number(n) => RequestId::from(n),
        lsp_types::NumberOrString::String(s) => RequestId::from(s),
    }
}

/// Deserialize a notification's params, logging rather than failing.
///
/// A client that sends a malformed notification is a client bug; taking the
/// server down over it would turn their bug into ours.
fn cast<N>(notification: Notification) -> Option<N::Params>
where
    N: lsp_types::notification::Notification,
{
    match notification.extract::<N::Params>(N::METHOD) {
        Ok(params) => Some(params),
        Err(ExtractError::JsonError { method, error }) => {
            tracing::warn!(method, %error, "malformed notification params");
            None
        }
        Err(ExtractError::MethodMismatch(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An owed refresh is sent once, and the next waits for its answer,
    /// since every refresh goes out under one id.
    #[test]
    fn a_hint_refresh_is_sent_once_until_the_client_answers_it() {
        let (server, client) = Connection::memory();
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.refreshes_hints();
        state.owe_hint_refresh();
        assert_eq!(send_hint_refresh(&server, &mut state), Flow::Continue);
        let Ok(Message::Request(sent)) = client.receiver.try_recv() else {
            panic!("a refresh request");
        };
        assert_eq!(sent.method, "workspace/inlayHint/refresh");
        state.owe_hint_refresh();
        send_hint_refresh(&server, &mut state);
        assert!(client.receiver.try_recv().is_err(), "one in flight");
        let answer = Response::new_ok(sent.id, serde_json::Value::Null);
        let mut in_flight = InFlight::new();
        handle_message(
            &server,
            &mut state,
            &mut in_flight,
            Message::Response(answer),
        );
        assert!(
            matches!(client.receiver.try_recv(), Ok(Message::Request(again)) if again.method == sent.method),
            "the refresh owed meanwhile goes out with the answer"
        );
    }

    fn state_with(uri: &str, source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(uri), 1, source.to_owned());
        state
    }

    fn request(id: i32, method: &str, params: serde_json::Value) -> Request {
        Request {
            id: RequestId::from(id),
            method: method.to_owned(),
            params,
        }
    }

    fn outline_request(id: i32, uri: &str) -> Request {
        request(
            id,
            DocumentSymbolRequest::METHOD,
            serde_json::json!({ "textDocument": { "uri": uri } }),
        )
    }

    fn error_code(response: &Response) -> i32 {
        match &response.response_result {
            Err(error) => error.code,
            Ok(value) => panic!("expected an error, got {value}"),
        }
    }

    #[test]
    fn an_outline_request_is_answered_from_the_open_document() {
        let mut state = state_with("file:///A.java", "class A { void go() {} }");
        let mut in_flight = InFlight::new();
        let response = handle_request(
            &mut state,
            &mut in_flight,
            outline_request(1, "file:///A.java"),
        );
        let symbols = response.response_result.expect("an outline");
        assert_eq!(symbols[0]["name"], "A");
        assert_eq!(symbols[0]["children"][0]["name"], "go");
    }

    #[test]
    fn a_cancelled_outline_answers_request_cancelled() {
        let mut state = state_with("file:///A.java", "class A {}");
        let token = CancellationToken::new();
        token.cancel();
        let response = route(&mut state, &token, outline_request(1, "file:///A.java"));
        assert_eq!(
            error_code(&response),
            lsp_server::ErrorCode::RequestCanceled as i32
        );
    }

    #[test]
    fn unroutable_and_malformed_requests_are_still_answered() {
        let mut state = state_with("file:///A.java", "class A {}");
        let token = CancellationToken::new();
        assert_eq!(
            error_code(&route(
                &mut state,
                &token,
                request(1, "textDocument/rename", serde_json::json!({}))
            )),
            lsp_server::ErrorCode::MethodNotFound as i32
        );
        let malformed = request(
            2,
            DocumentSymbolRequest::METHOD,
            serde_json::json!({ "textDocument": 7 }),
        );
        assert_eq!(
            error_code(&route(&mut state, &token, malformed)),
            lsp_server::ErrorCode::InvalidParams as i32
        );
    }

    #[test]
    fn a_watcher_is_registered_only_with_a_client_that_can_take_one() {
        let able = serde_json::json!({ "capabilities": { "workspace": {
            "didChangeWatchedFiles": { "dynamicRegistration": true } } } });
        let registration = watcher_registration(&able).expect("a registration");
        assert_eq!(registration.method, "client/registerCapability");
        assert_eq!(
            registration.params["registrations"][0]["registerOptions"]["watchers"][0]["globPattern"],
            "**/*.java"
        );
        let watchers = &registration.params["registrations"][0]["registerOptions"]["watchers"];
        assert_eq!(
            watchers[1]["globPattern"],
            "**/{pom.xml,*.gradle,*.gradle.kts,*.versions.toml,gradle.properties,\
             gradle-wrapper.properties,maven-wrapper.properties}",
            "every change to a build file, not only a create or a delete"
        );
        assert_eq!(watchers[1]["kind"], serde_json::Value::Null);
        for unable in [
            serde_json::json!({}),
            serde_json::json!({ "capabilities": { "workspace": {
                "didChangeWatchedFiles": { "dynamicRegistration": false } } } }),
        ] {
            assert!(watcher_registration(&unable).is_none(), "{unable}");
        }
    }

    #[test]
    fn no_answered_request_is_left_in_flight() {
        let mut state = state_with("file:///A.java", "class A {}");
        let mut in_flight = InFlight::new();
        for request in [
            outline_request(1, "file:///A.java"),
            outline_request(2, "file:///Absent.java"),
            request(3, "textDocument/rename", serde_json::json!({})),
            request(4, DocumentSymbolRequest::METHOD, serde_json::json!({})),
        ] {
            handle_request(&mut state, &mut in_flight, request);
        }
        assert!(in_flight.is_empty());
    }
}
