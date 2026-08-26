//! Boundary types between the Vue frontend and `rqs_lib`.
//!
//! The library's internal `ChannelMessage` is a nested enum
//! (`{ id, msg: Lib | Client }`), but the checked-in TypeScript bindings — and
//! therefore the Vue UI — expect the upstream *flat* shape
//! (`{ id, direction, action, rtype, state, meta }`). These DTOs reproduce that
//! flat shape and convert to/from the library's types, so the frontend is
//! unchanged while the library keeps its richer model.
use rqs_lib::DeviceType;
use rqs_lib::TransferState;
use rqs_lib::channel::{ChannelMessage, Message, MessageClient, TransferAction, TransferKind};
use rqs_lib::hdl::info::{TransferMetadata, TransferPayload};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub enum ChannelDirection {
    #[default]
    FrontToLib,
    LibToFront,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ChannelAction {
    AcceptTransfer,
    RejectTransfer,
    CancelTransfer,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TransferType {
    Inbound,
    Outbound,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FrontRemoteDeviceInfo {
    pub name: String,
    pub device_type: DeviceType,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FrontMeta {
    pub source: Option<FrontRemoteDeviceInfo>,
    pub pin_code: Option<String>,
    pub files: Option<Vec<String>>,
    pub text_description: Option<String>,
    pub text_payload: Option<String>,
    pub text_type: Option<String>,
    pub ack_bytes: u64,
    pub total_bytes: u64,
}

impl FrontMeta {
    fn from_lib(md: TransferMetadata) -> Self {
        let (files, text_description, text_payload, text_type) = match &md.payload {
            Some(TransferPayload::Files(v)) => (Some(v.clone()), None, None, None),
            Some(TransferPayload::Text(t)) => (
                None,
                md.payload_preview.clone(),
                Some(t.clone()),
                Some("Text".to_string()),
            ),
            Some(TransferPayload::Url(u)) => (
                None,
                md.payload_preview.clone(),
                Some(u.clone()),
                Some("Url".to_string()),
            ),
            Some(TransferPayload::Wifi { ssid, .. }) => {
                (None, Some(ssid.clone()), None, Some("Wifi".to_string()))
            }
            None => (None, md.payload_preview.clone(), None, None),
        };
        FrontMeta {
            source: md.source.map(|s| FrontRemoteDeviceInfo {
                name: s.name,
                device_type: s.device_type,
            }),
            pin_code: md.pin_code,
            files,
            text_description,
            text_payload,
            text_type,
            ack_bytes: md.ack_bytes,
            total_bytes: md.total_bytes,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FrontChannelMessage {
    pub id: String,
    #[serde(default)]
    pub direction: ChannelDirection,
    #[serde(default)]
    pub action: Option<ChannelAction>,
    #[serde(default)]
    pub rtype: Option<TransferType>,
    #[serde(default)]
    pub state: Option<TransferState>,
    #[serde(default)]
    pub meta: Option<FrontMeta>,
}

impl FrontChannelMessage {
    /// A library → frontend message (returns `None` for the frontend-directed
    /// `Lib` action variant, which is never emitted outward).
    pub fn from_lib(m: ChannelMessage) -> Option<Self> {
        let client: MessageClient = match m.msg {
            Message::Client(c) => c,
            Message::Lib { .. } => return None,
        };
        Some(FrontChannelMessage {
            id: m.id,
            direction: ChannelDirection::LibToFront,
            action: None,
            rtype: Some(match client.kind {
                TransferKind::Inbound => TransferType::Inbound,
                TransferKind::Outbound => TransferType::Outbound,
            }),
            state: client.state,
            meta: client.metadata.map(FrontMeta::from_lib),
        })
    }

    /// This message's state, if any (for the consent-notification check).
    pub fn lib_state(m: &ChannelMessage) -> Option<TransferState> {
        match &m.msg {
            Message::Client(c) => c.state.clone(),
            Message::Lib { .. } => None,
        }
    }

    /// The source device name of a library message, if any.
    pub fn lib_source_name(m: &ChannelMessage) -> Option<String> {
        match &m.msg {
            Message::Client(c) => c
                .metadata
                .as_ref()
                .and_then(|md| md.source.as_ref())
                .map(|s| s.name.clone()),
            Message::Lib { .. } => None,
        }
    }
}

/// Convert a frontend command into the library's internal message.
pub fn to_lib_message(fm: FrontChannelMessage) -> ChannelMessage {
    let action = match fm.action {
        Some(ChannelAction::AcceptTransfer) => TransferAction::ConsentAccept,
        Some(ChannelAction::RejectTransfer) => TransferAction::ConsentDecline,
        Some(ChannelAction::CancelTransfer) => TransferAction::TransferCancel,
        None => TransferAction::TransferCancel,
    };
    ChannelMessage {
        id: fm.id,
        msg: Message::Lib { action },
    }
}
