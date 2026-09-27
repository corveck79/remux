pub mod codecs;
pub mod provider_ids;
pub use codecs::{
    AudioCodec, AudioContainer, CodecProfileType, DlnaProfileType, SubtitleCodec,
    TranscodingProtocol, VideoCodec, VideoContainer,
};
pub use provider_ids::{AnyProviderIds, ExternalIdProvider};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use http::{HeaderValue, Method};
use nutype::nutype;
use remux_macros::{dto, query};
use serde::{Deserialize, Deserializer, Serialize};
use serde_alias::serde_alias;
use serde_aux::prelude::*;
use serde_with::{serde_as, skip_serializing_none};
use std::{collections::HashMap, str::FromStr};
use uuid::Uuid;

pub use crate::stremio::ResourceType;
use crate::{Auth, Body, Endpoint, RestClient, stremio};
