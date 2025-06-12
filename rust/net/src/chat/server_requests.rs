//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use bytes::Bytes;
use libsignal_net_infra::ws::WebSocketServiceError;
use libsignal_protocol::Timestamp;

use crate::chat::{ws, RequestProto, SendError};
use crate::env::TIMESTAMP_HEADER_NAME;

pub type ResponseEnvelopeSender =
    Box<dyn FnOnce(http::StatusCode) -> Result<(), SendError> + Send + Sync>;

pub enum ServerEvent {
    QueueEmpty,
    IncomingMessage {
        request_id: u64,
        envelope: Bytes,
        server_delivery_timestamp: Timestamp,
        send_ack: ResponseEnvelopeSender,
    },
    Alerts(Vec<String>),
    Stopped(DisconnectCause),
}

#[derive(Debug, derive_more::From)]
pub enum DisconnectCause {
    LocalDisconnect,
    Error(#[from] SendError),
}

impl std::fmt::Debug for ServerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueEmpty => write!(f, "QueueEmpty"),
            Self::IncomingMessage {
                envelope,
                server_delivery_timestamp,
                request_id,
                send_ack: _,
            } => f
                .debug_struct("IncomingMessage")
                .field("request_id", request_id)
                .field("envelope", &format_args!("{} bytes", envelope.len()))
                .field("server_delivery_timestamp", server_delivery_timestamp)
                .finish(),
            Self::Alerts(alerts) => f.debug_tuple("Alerts").field(&alerts.len()).finish(),
            Self::Stopped(error) => f
                .debug_struct("ConnectionInterrupted")
                .field("reason", error)
                .finish(),
        }
    }
}

#[derive(Debug, displaydoc::Display)]
pub enum ServerEventError {
    /// server request used unexpected verb {0}
    UnexpectedVerb(String),
    /// server request missing path
    MissingPath,
    /// server sent an unknown request: {0}
    UnrecognizedPath(String),
}

impl TryFrom<ws::ListenerEvent> for ServerEvent {
    type Error = ServerEventError;

    fn try_from(value: ws::ListenerEvent) -> Result<Self, Self::Error> {
        match value {
            ws::ListenerEvent::ReceivedAlerts(alerts) => Ok(Self::Alerts(alerts)),

            ws::ListenerEvent::ReceivedMessage(proto, responder) => {
                convert_received_message(proto, || {
                    Box::new(move |status| Ok(responder.send_response(status)?))
                })
            }

            ws::ListenerEvent::Finished(reason) => Ok(ServerEvent::Stopped(match reason {
                Ok(ws::FinishReason::LocalDisconnect) => DisconnectCause::LocalDisconnect,
                Ok(ws::FinishReason::RemoteDisconnect) => DisconnectCause::Error(
                    SendError::WebSocket(WebSocketServiceError::ChannelClosed),
                ),
                Err(ws::FinishError::Unknown) => DisconnectCause::Error(SendError::WebSocket(
                    WebSocketServiceError::Other("unexpected exit"),
                )),
                Err(ws::FinishError::Error(e)) => DisconnectCause::Error(e.into()),
            })),
        }
    }
}

fn convert_received_message(
    proto: crate::proto::chat_websocket::WebSocketRequestMessage,
    make_send_ack: impl FnOnce() -> ResponseEnvelopeSender,
) -> Result<ServerEvent, ServerEventError> {
    let RequestProto {
        verb,
        path,
        body,
        headers,
        id,
    } = proto;
    let verb = verb.unwrap_or_default();
    if verb != http::Method::PUT.as_str() {
        return Err(ServerEventError::UnexpectedVerb(verb));
    }

    let path = path.unwrap_or_default();
    match &*path {
        "/api/v1/queue/empty" => Ok(ServerEvent::QueueEmpty),
        "/api/v1/message" => {
            let raw_timestamp = headers
                .iter()
                .filter_map(|header| {
                    let (name, value) = header.split_once(':')?;
                    if name.eq_ignore_ascii_case(TIMESTAMP_HEADER_NAME) {
                        value.trim().parse::<u64>().ok()
                    } else {
                        None
                    }
                })
                .next_back();
            if raw_timestamp.is_none() {
                log::warn!("server delivered message with no {TIMESTAMP_HEADER_NAME} header");
            }
            let request_id = id.unwrap_or(0);

            // We don't check whether the body is missing here. The consumer still needs to ack
            // malformed envelopes, or they'd be delivered over and over, and an empty envelope
            // is just a special case of a malformed envelope.
            Ok(ServerEvent::IncomingMessage {
                request_id,
                envelope: body.unwrap_or_default(),
                server_delivery_timestamp: Timestamp::from_epoch_millis(
                    raw_timestamp.unwrap_or_default(),
                ),
                send_ack: make_send_ack(),
            })
        }
        "" => Err(ServerEventError::MissingPath),
        _unknown_path => Err(ServerEventError::UnrecognizedPath(path)),
    }
}
