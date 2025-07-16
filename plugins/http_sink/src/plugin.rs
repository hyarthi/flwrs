use crate::schema::build_schema;
use crate::schema::{BODY_KEY, HEADERS_KEY, METHOD_KEY, URL_KEY};
use bytes::Bytes;
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::common::log_level::Enum as LogLevel;
use flwrs_plugin::schema::schema::field_value::Value;
use flwrs_plugin::schema::schema::{Field, PluginPayload};
use flwrs_plugin::schema::sink::SinkEvent;
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
                return Err(SinkError {
                    source: Box::new(e),
                });
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

    fn consume_event(&mut self, event: SinkEvent) -> Result<(), SinkError> {
        if event.plugin_id != self.id {
            return Err(SinkError {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other, // TODO this should not be io
                    "Wrong plugin ID",
                )),
            });
        }
        if event.plugin_version != VERSION {
            return Err(SinkError {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other, // TODO this should not be io
                    "Wrong plugin version",
                )),
            });
        }
        let payload = event.payload.ok_or(SinkError {
            source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "No payload")), // TODO this should not be io
        })?;

        log::trace!("Received event: {:?}", payload);
        let request = self.build_request(payload)?;

        tokio::task::spawn(send_request(request));

        Ok(())
    }
}

async fn send_request(request: RequestBuilder) {
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            log::error!("Error sending request: {}", e);
            return;
        }
    };

    match response.error_for_status() {
        Ok(_) => log::trace!("Request sent successfully"),
        Err(e) => log::error!("Error sending request: {}", e),
    };
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
                    _ => Err(SinkError {
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "Unsupported URL value type",
                        )),
                    }),
                },
            },
        }
    }

    fn parse_method(field: &Field) -> Result<Method, SinkError> {
        match &field.value {
            None => Ok(Method::GET),
            Some(value) => match &value.value {
                None => Ok((Method::GET)),
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
                        _ => Err(SinkError {
                            source: Box::new(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                "Unsupported method value",
                            )),
                        }),
                    },
                    _ => Err(SinkError {
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "Unsupported method value type",
                        )),
                    }),
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
                                                _ => Err(SinkError {
                                                    source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Unsupported headers value value type")),
                                                })
                                            }
                                        })?;
                                        headers.insert(key.clone(), array);
                                        Ok(())
                                    }
                                    _ => Err(SinkError {
                                        source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Unsupported headers value value type")),
                                    })
                                }
                            })?;
                            Ok(headers)
                        }
                        _ => Err(SinkError {
                            source: Box::new(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                "Unsupported headers value type",
                            )),
                        }),
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
                    _ => Err(SinkError {
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "Unsupported body value type",
                        )),
                    }),
                },
            },
        }
    }
}
