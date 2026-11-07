use crate::schema::build_schema;
use crate::schema::{BODY_KEY, HEADERS_KEY, METHOD_KEY, URL_KEY};
use bytes::Bytes;
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::schema::payload::field_value::Value;
use flwrs_plugin::plugin::core::InboundEvent;
use flwrs_plugin::schema::payload::{Field, PluginPayload};
use flwrs_plugin::sink::plugin::Sink;
use reqwest::{Client, ClientBuilder, Method, RequestBuilder};
use std::collections::HashMap;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

pub(crate) struct PluginSettings {
    pub connect_timeout: Duration,
    pub verbose_logging: bool,
    pub read_timeout: Duration,
    pub timeout: Duration,
}

pub(crate) struct Plugin {
    id: String,
    client: Client,
}

impl Plugin {
    pub fn new(id: &str, settings: PluginSettings) -> Result<Self, SinkError> {
        let client = match ClientBuilder::new()
            .connect_timeout(settings.connect_timeout)
            .connection_verbose(settings.verbose_logging)
            .read_timeout(settings.read_timeout)
            .timeout(settings.timeout)
            .user_agent(APP_USER_AGENT)
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                return Err(SinkError::new(e));
            }
        };
        Ok(Self {
            id: id.to_string(),
            client,
        })
    }

    fn build_request(&self, payload: PluginPayload) -> Result<RequestBuilder, SinkError> {
        let parsed_payload = ParsedPayload::parse(payload)?;
        let mut request = self
            .client
            .request(parsed_payload.method, parsed_payload.url);
        for (key, values) in parsed_payload.headers.iter() {
            for value in values {
                request = request.header(key, value);
            }
        }
        request = request.body(parsed_payload.body);
        Ok(request)
    }
}

impl Sink for Plugin {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        self.id = plugin_id;
        Ok(InitializeRequest::new()
            .with_id(self.id.clone())
            .with_version(VERSION.to_string())
            .with_schema(build_schema()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        // noop
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    /// Sends the request and waits for the response: the event is
    /// acknowledged once the server answered with a success status. A
    /// failure is rejected, and retried when it may go away (see [`classify`]).
    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        let payload = event.payload;
        log::trace!("Received event (attempt {}): {:?}", event.attempt, payload);
        let request = self.build_request(payload)?;
        let response = request.send().await.map_err(classify)?;
        response.error_for_status().map_err(classify)?;
        log::trace!("Request sent successfully");
        Ok(())
    }
}

/// Whether a failed request is worth retrying: network errors, timeouts,
/// 408, 429 and 5xx are; an invalid request or another 4xx isn't.
fn classify(err: reqwest::Error) -> SinkError {
    let retry = match err.status() {
        Some(status) => {
            status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
        }
        None => !err.is_builder(),
    };
    let message = format!("HTTP request failed: {err}");
    if retry {
        SinkError::retryable(message)
    } else {
        SinkError::new(message)
    }
}

struct ParsedPayload {
    url: String,
    method: Method,
    headers: HashMap<String, Vec<String>>,
    body: Bytes,
}

impl ParsedPayload {
    fn parse(payload: PluginPayload) -> Result<Self, SinkError> {
        let mut url = String::new();
        let mut method = Method::GET;
        let mut headers = HashMap::new();
        let mut body = Bytes::new();

        payload
            .fields
            .iter()
            .try_for_each(|field| match field.key.as_str() {
                URL_KEY => {
                    url = Self::parse_url(field)?;
                    Ok(())
                }
                METHOD_KEY => {
                    method = Self::parse_method(field)?;
                    Ok(())
                }
                HEADERS_KEY => {
                    headers = Self::parse_headers(field)?;
                    Ok(())
                }
                BODY_KEY => {
                    body = Self::parse_body(field)?;
                    Ok(())
                }
                _ => Ok(()),
            })?;

        Ok(Self {
            url,
            method,
            headers,
            body,
        })
    }

    fn parse_url(field: &Field) -> Result<String, SinkError> {
        match &field.value {
            None => Ok("".to_string()),
            Some(value) => match &value.value {
                None => Ok("".to_string()),
                Some(value) => match value {
                    Value::String(uri) => Ok(uri.clone()),
                    _ => Err(SinkError::new("Unsupported URL value type")),
                },
            },
        }
    }

    fn parse_method(field: &Field) -> Result<Method, SinkError> {
        match &field.value {
            None => Ok(Method::GET),
            Some(value) => match &value.value {
                None => Ok(Method::GET),
                Some(value) => match value {
                    Value::String(m) => match m.as_str() {
                        "GET" => Ok(Method::GET),
                        "HEAD" => Ok(Method::HEAD),
                        "POST" => Ok(Method::POST),
                        "PUT" => Ok(Method::PUT),
                        "PATCH" => Ok(Method::PATCH),
                        "DELETE" => Ok(Method::DELETE),
                        "OPTIONS" => Ok(Method::OPTIONS),
                        "CONNECT" => Ok(Method::CONNECT),
                        "TRACE" => Ok(Method::TRACE),
                        _ => Err(SinkError::new("Unsupported method value")),
                    },
                    _ => Err(SinkError::new("Unsupported method value type")),
                },
            },
        }
    }

    fn parse_headers(field: &Field) -> Result<HashMap<String, Vec<String>>, SinkError> {
        let mut headers = HashMap::new();

        match &field.value {
            None => Ok(headers),
            Some(value) => {
                match &value.value {
                    None => Ok(headers),
                    Some(value) => match value {
                        Value::Map(map) => {
                            map.value.iter().try_for_each(|(key, value)| match &value.value {
                                None => Ok(()),
                                Some(value) => match value {
                                    Value::Array(value) => {
                                        let mut array = vec![];
                                        value.value.iter().try_for_each(|item| match &item.value {
                                            None => Ok(()),
                                            Some(value) => match value {
                                                Value::String(s) => {
                                                    array.push(s.clone());
                                                    Ok(())
                                                }
                                                _ => Err(SinkError::new("Unsupported headers value value type"))
                                            }
                                        })?;
                                        headers.insert(key.clone(), array);
                                        Ok(())
                                    }
                                    _ => Err(SinkError::new("Unsupported headers value value type"))
                                }
                            })?;
                            Ok(headers)
                        }
                        _ => Err(SinkError::new("Unsupported headers value type")),
                    },
                }
            }
        }
    }

    fn parse_body(field: &Field) -> Result<Bytes, SinkError> {
        match &field.value {
            None => Ok(Bytes::new()),
            Some(value) => match &value.value {
                None => Ok(Bytes::new()),
                Some(value) => match value {
                    Value::String(s) => Ok(Bytes::from(s.clone().into_bytes())),
                    Value::Bytes(b) => Ok(Bytes::from(b.clone())),
                    _ => Err(SinkError::new("Unsupported body value type")),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin::schema::payload::{ArrayValue, FieldValue, MapValue};

    fn field(key: &str, value: Value) -> Field {
        Field {
            key: key.into(),
            value: Some(FieldValue { value: Some(value) }),
        }
    }

    fn string(s: &str) -> Value {
        Value::String(s.into())
    }

    #[test]
    fn parses_full_payload() {
        let headers = MapValue {
            value: [(
                "accept".to_string(),
                FieldValue {
                    value: Some(Value::Array(ArrayValue {
                        value: vec![
                            FieldValue {
                                value: Some(string("text/plain")),
                            },
                            FieldValue {
                                value: Some(string("application/json")),
                            },
                        ],
                    })),
                },
            )]
            .into(),
        };
        let parsed = ParsedPayload::parse(PluginPayload {
            fields: vec![
                field(URL_KEY, string("http://example.com/x")),
                field(METHOD_KEY, string("POST")),
                field(HEADERS_KEY, Value::Map(headers)),
                field(BODY_KEY, Value::Bytes(b"raw".to_vec())),
                field("ignored", string("whatever")),
            ],
        })
        .unwrap();

        assert_eq!(parsed.url, "http://example.com/x");
        assert_eq!(parsed.method, Method::POST);
        assert_eq!(
            parsed.headers["accept"],
            vec!["text/plain", "application/json"]
        );
        assert_eq!(parsed.body, Bytes::from_static(b"raw"));
    }

    #[test]
    fn defaults_to_get_with_empty_body() {
        let parsed = ParsedPayload::parse(PluginPayload {
            fields: vec![field(URL_KEY, string("http://example.com"))],
        })
        .unwrap();
        assert_eq!(parsed.method, Method::GET);
        assert!(parsed.body.is_empty());
        assert!(parsed.headers.is_empty());
    }

    #[test]
    fn rejects_bad_values() {
        let bad = |f: Field| ParsedPayload::parse(PluginPayload { fields: vec![f] }).is_err();
        assert!(bad(field(METHOD_KEY, string("FETCH"))));
        assert!(bad(field(URL_KEY, Value::Bool(true))));
        assert!(bad(field(HEADERS_KEY, string("nope"))));
    }
}
