use api_req::{ApiCaller, Method, Payload, header};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

use crate::account::AuthData;
use crate::error::{Result, XiaoaiErr};
use crate::serde_util;

/// Find a device by its alias. Only works for the device's *owner* — an
/// administrator cannot query it.
pub async fn device_by_alias(auth_data: &AuthData, alias: impl AsRef<str>) -> Result<Device> {
    let payload = DeviceListPayload::new(auth_data);
    let resp: DeviceListResponse = OpApi::request(payload)
        .await
        .map_err(|e| XiaoaiErr::Op(format!("Device list query failed: {e}")))?;
    resp.data
        .iter()
        .find(|d| d.alias == alias.as_ref())
        .cloned()
        .ok_or_else(|| {
            XiaoaiErr::Op(format!(
                "Device {} not found in {}",
                alias.as_ref(),
                resp.data
                    .into_iter()
                    .map(|d| d.alias)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Every request carries a fresh opaque request id.
fn request_id() -> String {
    format!(
        "app_ios_{}",
        Alphanumeric.sample_string(&mut rand::rng(), 30)
    )
}

/// Operations on a speaker; every op POSTs to `/remote/ubus`.
#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = "https://api2.mina.mi.com",
    default_headers = [
        (header::USER_AGENT, "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103"),
    ]
)]
pub struct OpApi {}

#[derive(Debug, Serialize, Payload)]
#[api_req(
    path = "/admin/v2/device_list",
    method = Method::GET,
    headers = [(header::COOKIE, "userId={user_id}; serviceToken={service_token}")],
    req = query
)]
pub struct DeviceListPayload {
    #[serde(skip_serializing)]
    user_id: i64,
    #[serde(skip_serializing)]
    service_token: String,
    master: i64,
    #[serde(rename = "requestId")]
    request_id: String,
}

impl DeviceListPayload {
    fn new(auth_data: &AuthData) -> Self {
        Self {
            user_id: auth_data.user_id,
            service_token: auth_data.service_token.to_owned(),
            master: 0,
            request_id: request_id(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeviceListResponse {
    data: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub alias: String,
    #[serde(rename = "deviceID")]
    pub device_id: String,
    pub hardware: String,
    #[serde(flatten)]
    pub others: serde_json::Value,
}

/// One ubus operation; build it with [`OpPayloadBuilder`].
#[derive(Debug, Serialize, Payload)]
#[api_req(
    path = "/remote/ubus",
    method = Method::POST,
    headers = [(header::COOKIE, "userId={user_id}; serviceToken={service_token}")],
    req = form
)]
pub struct OpPayload<T>
where
    T: Send + Sync + Serialize + 'static,
{
    #[serde(skip_serializing)]
    user_id: i64,
    #[serde(skip_serializing)]
    service_token: String,
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(flatten)]
    op: Op<T>,
}

#[derive(Debug, Serialize)]
pub struct Op<T>
where
    T: Send + Sync + Serialize + 'static,
{
    method: String,
    path: String,
    #[serde(serialize_with = "serde_util::to_string")]
    message: T,
}

#[derive(Debug)]
pub struct OpPayloadBuilder {
    user_id: i64,
    service_token: String,
    request_id: String,
    device_id: String,
}

impl Default for OpPayloadBuilder {
    fn default() -> Self {
        Self {
            user_id: 0,
            service_token: String::new(),
            request_id: request_id(),
            device_id: String::new(),
        }
    }
}

impl OpPayloadBuilder {
    pub fn new(auth_data: &AuthData, device_id: impl AsRef<str>) -> Self {
        Self {
            user_id: auth_data.user_id,
            service_token: auth_data.service_token.to_owned(),
            request_id: request_id(),
            device_id: device_id.as_ref().to_string(),
        }
    }

    pub fn with_auth_data(mut self, auth_data: AuthData) -> Self {
        self.user_id = auth_data.user_id;
        self.service_token = auth_data.service_token;
        self
    }

    pub fn with_device_id(mut self, device_id: String) -> Self {
        self.device_id = device_id;
        self
    }

    /// Wrap one ubus call in the envelope every operation shares.
    fn build<T>(self, path: &str, method: &str, message: T) -> OpPayload<T>
    where
        T: Send + Sync + Serialize + 'static,
    {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: method.to_string(),
                path: path.to_string(),
                message,
            },
        }
    }

    pub fn speak(self, text: impl AsRef<str>) -> OpPayload<Speak> {
        let text = text.as_ref().to_string();
        self.build("mibrain", "text_to_speech", Speak { text })
    }

    pub fn volume(self, volume: usize) -> OpPayload<Volume> {
        self.build("mediaplayer", "player_set_volume", Volume { volume })
    }

    pub fn pause(self) -> OpPayload<Play> {
        let action = "pause".to_string();
        self.build("mediaplayer", "player_play_operation", Play { action })
    }

    /// Resume playback.
    pub fn play(self) -> OpPayload<Play> {
        let action = "play".to_string();
        self.build("mediaplayer", "player_play_operation", Play { action })
    }

    /// Playback status; see [`XiaoaiStatus`].
    pub fn status(self) -> OpPayload<Status> {
        self.build("mediaplayer", "player_get_play_status", Status {})
    }

    pub fn play_url(self, url: impl AsRef<str>) -> OpPayload<PlayUrl> {
        let url = url.as_ref().to_string();
        self.build("mediaplayer", "player_play_url", PlayUrl { url, r#type: 1 })
    }
}

#[derive(Debug, Serialize)]
pub struct Speak {
    text: String,
}

#[derive(Debug, Serialize)]
pub struct Volume {
    volume: usize,
}

#[derive(Debug, Serialize)]
pub struct Play {
    action: String,
}

#[derive(Debug, Serialize)]
pub struct Status {}

#[derive(Debug, Serialize)]
pub struct PlayUrl {
    url: String,
    r#type: usize,
}

#[derive(Debug, Deserialize)]
pub struct OpResponse {
    pub data: OpData,
}

impl OpResponse {
    /// Playback status, or [`XiaoaiStatus::Unknown`] if the response carried none
    /// (which is the case for every operation other than `status`).
    pub fn status(&self) -> XiaoaiStatus {
        self.data
            .info
            .as_ref()
            .and_then(|info| info.status)
            .map_or(XiaoaiStatus::Unknown, XiaoaiStatus::from_code)
    }
}

/// Playback status of the speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiaoaiStatus {
    Idle,
    Playing,
    Paused,
    Stopped,
    Unknown,
}

impl XiaoaiStatus {
    fn from_code(code: usize) -> Self {
        match code {
            0 => Self::Idle,
            1 => Self::Playing,
            2 => Self::Paused,
            3 => Self::Stopped,
            _ => Self::Unknown,
        }
    }
}

impl Deref for OpResponse {
    type Target = OpData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug, Deserialize)]
pub struct OpData {
    #[serde(deserialize_with = "serde_util::from_string", default)]
    pub info: Option<Info>,
}

#[derive(Debug, Deserialize)]
pub struct Info {
    pub status: Option<usize>,
    pub volume: Option<usize>,
    pub loop_type: Option<usize>,
    pub path: Option<String>,
    #[serde(flatten)]
    pub others: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::load_or_login_and_save_with_env;
    use crate::device_by_alias;
    use crate::op::{OpApi, OpPayloadBuilder};

    #[tokio::test]
    async fn test_ops() {
        let auth_data = load_or_login_and_save_with_env(crate::AUTH_DATA_PATH)
            .await
            .unwrap();
        let device = device_by_alias(&auth_data, "哈哈").await.unwrap();
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).status();
        let _: OpResponse = OpApi::request(payload).await.unwrap();
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).volume(40);
        let _: OpResponse = OpApi::request(payload).await.unwrap();
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).speak("我是奶龙");
        let _: OpResponse = OpApi::request(payload).await.unwrap();
    }
}
