//! Responses API (SSE streaming) and model list, authenticated with a ChatGPT plan access token.

use std::io::BufRead;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::prelude::{BASE64_STANDARD, Engine};
use serde_json::{Value, json};
use ureq::http::Response;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Body};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODELS_URL: &str = "https://api.openai.com/v1/models";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// `image/*` is sent as `input_image`, anything else (PDF etc.) as `input_file`.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub instructions: Option<&'a str>,
    pub turns: &'a [Turn],
    pub web_search: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Text(String),
    Searching,
    SearchDone,
}

/// `status` is set only for non-2xx HTTP responses; `status` and `code` both empty means a network failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: String,
    pub param: Option<String>,
}

impl ApiError {
    pub fn is_usage_limit(&self) -> bool {
        self.code.as_deref() == Some("subscription_sharing_usage_limit_exceeded")
    }

    pub fn is_auth(&self) -> bool {
        self.status == Some(401)
    }

    pub fn is_transient(&self) -> bool {
        self.status == Some(503) || (self.status.is_none() && self.code.is_none())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// GET /v1/models, keeping `visibility == "list"` in server order.
pub fn list_models(token: &str) -> Result<Vec<Model>, ApiError> {
    let mut resp = check(
        agent()
            .get(MODELS_URL)
            .config()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .call(),
    )?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(network)?;
    parse_models(&body).ok_or_else(|| ApiError {
        status: Some(status),
        code: None,
        message: "The model list could not be read.".into(),
        param: None,
    })
}

pub fn request_body(req: &Request) -> Value {
    let input: Vec<Value> = req
        .turns
        .iter()
        // An empty assistant turn (stopped before any text) would be rejected by the API.
        .filter(|t| t.role == Role::User || !t.text.is_empty())
        .map(|t| match t.role {
            Role::Assistant => json!({ "role": "assistant", "content": t.text }),
            Role::User => {
                let mut content = Vec::new();
                if !t.text.is_empty() {
                    content.push(json!({ "type": "input_text", "text": t.text }));
                }
                for a in &t.attachments {
                    let url = format!("data:{};base64,{}", a.mime, BASE64_STANDARD.encode(&a.data));
                    content.push(if a.mime.starts_with("image/") {
                        json!({ "type": "input_image", "image_url": url })
                    } else {
                        json!({ "type": "input_file", "filename": a.name, "file_data": url })
                    });
                }
                json!({ "role": "user", "content": content })
            }
        })
        .collect();
    let mut body = json!({ "model": req.model, "input": input, "store": false, "stream": true });
    if let Some(i) = req.instructions.filter(|i| !i.trim().is_empty()) {
        body["instructions"] = i.into();
    }
    if req.web_search {
        body["tools"] = json!([{ "type": "web_search" }]);
    }
    body
}

/// POST /v1/responses and feed stream events to `on_event` until `response.completed`.
/// Returns `Ok(())` early when `cancel` is set (checked between events).
pub fn stream(
    token: &str,
    req: &Request,
    cancel: &AtomicBool,
    on_event: impl FnMut(Event),
) -> Result<(), ApiError> {
    let resp = check(
        agent()
            .post(RESPONSES_URL)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .send(request_body(req).to_string()),
    )?;
    read_events(
        std::io::BufReader::new(resp.into_body().into_reader()),
        cancel,
        on_event,
    )
}

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls)
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            // ponytail: one 10 min cap for the body; no per-read timeout since reasoning models go quiet between events.
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into()
    })
}

fn check(resp: Result<Response<Body>, ureq::Error>) -> Result<Response<Body>, ApiError> {
    let mut resp = resp.map_err(network)?;
    if resp.status().is_success() {
        return Ok(resp);
    }
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    Err(error_from_body(resp.status().as_u16(), &body))
}

fn network(e: impl std::fmt::Display) -> ApiError {
    ApiError {
        status: None,
        code: None,
        message: format!("Network error: {e}"),
        param: None,
    }
}

/// Body is `{"error":{code,message,param}}` or `{"detail":".."}`.
fn error_from_body(status: u16, body: &str) -> ApiError {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let e = v.get("error").or_else(|| v.get("detail"));
    error_from(Some(status), e.unwrap_or(&Value::Null))
}

/// `e` is an error object `{code,message,param}` or a plain message string.
fn error_from(status: Option<u16>, e: &Value) -> ApiError {
    let field = |k: &str| e[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let message = e
        .as_str()
        .map(str::to_owned)
        .or_else(|| field("message"))
        .unwrap_or_else(|| match status {
            Some(s) => format!("The request failed (HTTP {s})."),
            None => "The response failed.".into(),
        });
    ApiError {
        status,
        code: field("code"),
        message,
        param: field("param"),
    }
}

fn parse_models(body: &str) -> Option<Vec<Model>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let models = v["models"].as_array()?.iter();
    Some(
        models
            .filter(|m| m["visibility"] == "list")
            .filter_map(|m| {
                let slug = m["slug"].as_str()?;
                let name = m["display_name"].as_str().unwrap_or(slug);
                Some(Model {
                    slug: slug.into(),
                    display_name: name.into(),
                })
            })
            .collect(),
    )
}

fn read_events(
    reader: impl BufRead,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> Result<(), ApiError> {
    let mut data = String::new();
    // The trailing empty line dispatches a final event the server did not terminate.
    for line in reader.lines().chain([Ok(String::new())]) {
        let line = line.map_err(network)?;
        if !line.is_empty() {
            if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
                data.push('\n');
            }
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if data.pop().is_some()
            && let Some(done) = handle_event(&data, &mut on_event)
        {
            return done;
        }
        data.clear();
    }
    Err(ApiError {
        message: "The response ended early.".into(),
        ..network("")
    })
}

/// `Some` ends the stream; unknown or malformed events are ignored.
fn handle_event(data: &str, on_event: &mut impl FnMut(Event)) -> Option<Result<(), ApiError>> {
    let v: Value = serde_json::from_str(data).ok()?;
    match v["type"].as_str()? {
        "response.output_text.delta" => on_event(Event::Text(v["delta"].as_str()?.into())),
        "response.web_search_call.in_progress" | "response.web_search_call.searching" => {
            on_event(Event::Searching)
        }
        "response.web_search_call.completed" => on_event(Event::SearchDone),
        "response.completed" => return Some(Ok(())),
        "response.failed" => return Some(Err(error_from(None, &v["response"]["error"]))),
        "response.incomplete" => {
            let reason = v["response"]["incomplete_details"]["reason"].as_str();
            return Some(Err(ApiError {
                status: None,
                code: Some(reason.unwrap_or("incomplete").into()),
                message: "The response is incomplete.".into(),
                param: None,
            }));
        }
        "error" => {
            let e = v.get("error").filter(|e| e.is_object()).unwrap_or(&v);
            return Some(Err(error_from(None, e)));
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str, attachments: Vec<Attachment>) -> Turn {
        Turn {
            role: Role::User,
            text: text.into(),
            attachments,
        }
    }

    fn req<'a>(turns: &'a [Turn], instructions: Option<&'a str>, web_search: bool) -> Request<'a> {
        Request {
            model: "gpt-5",
            instructions,
            turns,
            web_search,
        }
    }

    fn file(name: &str, mime: &str) -> Attachment {
        Attachment {
            name: name.into(),
            mime: mime.into(),
            data: vec![1, 2, 3],
        }
    }

    #[test]
    fn body_text_only() {
        let turns = [
            user("Hi", vec![]),
            Turn {
                role: Role::Assistant,
                text: "Hello".into(),
                attachments: vec![],
            },
            Turn {
                role: Role::Assistant,
                text: String::new(),
                attachments: vec![],
            },
            user("Bye", vec![]),
        ];
        assert_eq!(
            request_body(&req(&turns, None, false)),
            json!({
                "model": "gpt-5",
                "input": [
                    { "role": "user", "content": [{ "type": "input_text", "text": "Hi" }] },
                    { "role": "assistant", "content": "Hello" },
                    { "role": "user", "content": [{ "type": "input_text", "text": "Bye" }] },
                ],
                "store": false,
                "stream": true,
            })
        );
    }

    #[test]
    fn body_image_and_pdf() {
        let turns = [user(
            "Look",
            vec![file("a.png", "image/png"), file("b.pdf", "application/pdf")],
        )];
        assert_eq!(
            request_body(&req(&turns, None, false))["input"][0]["content"],
            json!([
                { "type": "input_text", "text": "Look" },
                { "type": "input_image", "image_url": "data:image/png;base64,AQID" },
                { "type": "input_file", "filename": "b.pdf", "file_data": "data:application/pdf;base64,AQID" },
            ])
        );
        // No empty text part when only an attachment is sent.
        let turns = [user("", vec![file("a.jpg", "image/jpeg")])];
        let body = request_body(&req(&turns, None, false));
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn body_instructions_and_tools() {
        let turns = [user("Hi", vec![])];
        for empty in [None, Some(""), Some("  \n")] {
            assert!(
                request_body(&req(&turns, empty, false))
                    .get("instructions")
                    .is_none()
            );
        }
        let body = request_body(&req(&turns, Some("Be brief"), true));
        assert_eq!(body["instructions"], "Be brief");
        assert_eq!(body["tools"], json!([{ "type": "web_search" }]));
        assert!(
            request_body(&req(&turns, None, false))
                .get("tools")
                .is_none()
        );
    }

    #[test]
    fn body_has_no_forbidden_keys() {
        let turns = [
            user("Hi", vec![file("a.png", "image/png")]),
            Turn {
                role: Role::Assistant,
                text: "Hello".into(),
                attachments: vec![],
            },
        ];
        let allowed = ["model", "input", "instructions", "store", "stream", "tools"];
        for (instructions, web) in [(None, false), (Some("x"), true)] {
            let body = request_body(&req(&turns, instructions, web));
            for key in body.as_object().unwrap().keys() {
                assert!(allowed.contains(&key.as_str()), "unexpected key {key}");
            }
            for item in body["input"].as_array().unwrap() {
                assert_ne!(item["role"], "system");
            }
        }
    }

    fn run(sse: &str) -> (Vec<Event>, Result<(), ApiError>) {
        let mut events = Vec::new();
        let result = read_events(sse.as_bytes(), &AtomicBool::new(false), |e| events.push(e));
        (events, result)
    }

    #[test]
    fn sse_deltas_and_completed() {
        let sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
                   : keep-alive\n\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\r\n\r\n\
                   data: {\"type\":\"response.web_search_call.in_progress\"}\n\n\
                   data: {\"type\":\"response.web_search_call.searching\"}\n\n\
                   data: {\"type\":\"response.web_search_call.completed\"}\n\n\
                   data: {\"type\":\"response.some_future_event\"}\n\n\
                   data: not json\n\n\
                   data: {\"type\":\"response.output_text.delta\",\n\
                   data: \"delta\":\"lo\"}\n\n\
                   data: {\"type\":\"response.completed\"}\n\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"ignored\"}\n\n";
        let (events, result) = run(sse);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            [
                Event::Text("Hel".into()),
                Event::Searching,
                Event::Searching,
                Event::SearchDone,
                Event::Text("lo".into()),
            ]
        );
        // A final event without its terminating blank line still counts.
        assert_eq!(run("data: {\"type\":\"response.completed\"}").1, Ok(()));
    }

    #[test]
    fn sse_failures() {
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"Limit hit\"}}}\n\n";
        let err = run(sse).1.unwrap_err();
        assert!(err.is_usage_limit() && !err.is_transient());
        assert_eq!(err.message, "Limit hit");

        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        let err = run(sse).1.unwrap_err();
        assert_eq!(err.code.as_deref(), Some("max_output_tokens"));
        assert!(!err.is_transient());

        let sse = "data: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Oops\",\"param\":null}\n\n";
        let err = run(sse).1.unwrap_err();
        assert_eq!(
            (err.code.as_deref(), err.message.as_str()),
            (Some("server_error"), "Oops")
        );

        let (events, result) =
            run("data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n");
        assert_eq!(events, [Event::Text("Hi".into())]);
        let err = result.unwrap_err();
        assert_eq!(err.message, "The response ended early.");
        assert!(err.is_transient());
    }

    #[test]
    fn sse_cancel_stops_early() {
        let mut events = Vec::new();
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n";
        let result = read_events(sse.as_bytes(), &AtomicBool::new(true), |e| events.push(e));
        assert_eq!(result, Ok(()));
        assert!(events.is_empty());
    }

    #[test]
    fn error_body_parsing() {
        let err = error_from_body(
            400,
            r#"{"error":{"code":"subscription_sharing_unsupported_capability","message":"No tools","param":"tools"}}"#,
        );
        assert_eq!(
            err,
            ApiError {
                status: Some(400),
                code: Some("subscription_sharing_unsupported_capability".into()),
                message: "No tools".into(),
                param: Some("tools".into()),
            }
        );
        let err = error_from_body(401, r#"{"detail":"Unauthorized"}"#);
        assert!(err.is_auth());
        assert_eq!(err.message, "Unauthorized");
        let err = error_from_body(503, "<html>busy</html>");
        assert!(err.is_transient() && err.code.is_none());
        assert_eq!(err.message, "The request failed (HTTP 503).");
        assert!(
            error_from_body(
                429,
                r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#
            )
            .is_usage_limit()
        );
    }

    #[test]
    fn models_keep_listed_in_order() {
        let body = r#"{"models":[
            {"slug":"b","display_name":"B","visibility":"list"},
            {"slug":"hidden","display_name":"H","visibility":"hide"},
            {"slug":"a","visibility":"list"}]}"#;
        let models = parse_models(body).unwrap();
        let slugs: Vec<_> = models
            .iter()
            .map(|m| (m.slug.as_str(), m.display_name.as_str()))
            .collect();
        assert_eq!(slugs, [("b", "B"), ("a", "a")]);
        assert!(parse_models("{}").is_none());
    }
}
