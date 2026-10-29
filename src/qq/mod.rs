pub(crate) mod api;
pub(crate) mod directive;
pub(crate) mod gateway;
pub(crate) mod group;
pub mod message_content;
pub(crate) mod render;
pub(crate) mod types;

// Façade: the symbols the rest of the crate reaches for, re-exported so callers
// import `crate::qq::X` instead of spelling out the submodule layout. The `pub`
// group is what the binary wires up; the rest is crate-internal.
pub use api::{
    GroupNotSentReason, GroupSendError, GroupSendUnknownReason, ImageUploadError, MaterialError,
    MaterialImage, QqApiClient, QqReadError, QqReadRequest, UploadedImage,
};
pub use gateway::{
    GroupGatewayEvent, InboundGatewayEvent, InboundMessage, spawn_chat_gateway, spawn_gateway,
    spawn_group_gateway,
};
pub use group::{
    GroupAttachment, GroupMessageAck, GroupMessageAckExt, GroupMessageElement, GroupMessageEvent,
    GroupMessageScene, GroupMessageSource, GroupQuote, GroupUser,
};
pub use types::C2CMessageEvent;

pub(crate) use directive::{Directive, parse_output};
pub(crate) use render::{PassiveDispatchReport, PassiveTurnEmitter};
pub(crate) use types::{MSG_TYPE_QUOTE, MessageAttachment, MsgElement};
