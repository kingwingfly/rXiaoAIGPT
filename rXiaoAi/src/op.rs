use api_req::{ApiCaller, Method, Payload, header};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize, Serializer};
use std::ops::Deref;

use crate::account::AuthData;
use crate::error::{Result, XiaoAiErr};

/// Query device of account by alias
///
/// Must be the owner of the device, even administator is unable to query device
pub async fn device_by_alias(auth_data: &AuthData, alias: impl AsRef<str>) -> Result<Device> {
    let payload = DeviceListPayload::new(auth_data);
    let resp: DeviceListResponse = OpApi::request(payload).await.unwrap();
    resp.data
        .iter()
        .find(|d| d.alias == alias.as_ref())
        .cloned()
        .ok_or_else(|| {
            XiaoAiErr::Op(format!(
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

/// Op API caller, pass it a payload and it will return a future, implmented by `api_req` macro
#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = "https://api2.mina.mi.com",
    default_headers = (
        (header::USER_AGENT, "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103"),
    )
)]
pub struct OpApi {}

#[derive(Debug, Serialize, Payload)]
#[api_req(
    path = "/admin/v2/device_list",
    method = Method::GET,
    headers = ((header::COOKIE, "userId={user_id}; serviceToken={service_token}"), ),
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
            request_id: format!(
                "app_ios_{}",
                Alphanumeric.sample_string(&mut rand::rng(), 30)
            ),
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

/// Operation payload, build it with `OpPayloadBuilder`
#[derive(Debug, Serialize, Payload)]
#[api_req(
    path = "/remote/ubus",
    method = Method::POST,
    headers = ((header::COOKIE, "userId={user_id}; serviceToken={service_token}"), ),
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
    #[serde(serialize_with = "serde_to_string")]
    message: T,
}

fn serde_to_string<T, S>(value: &T, serializer: S) -> core::result::Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    let res = serde_json::to_string(value)
        .map_err(|e| serde::ser::Error::custom(format!("Failed to serialize message {}", e)))?;
    res.serialize(serializer)
}

/// Operation payload builder, use it to build a payload
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
            request_id: format!(
                "app_ios_{}",
                Alphanumeric.sample_string(&mut rand::rng(), 30)
            ),
            device_id: String::new(),
        }
    }
}

impl OpPayloadBuilder {
    /// Create a new builder with auth data to operate on device id
    pub fn new(auth_data: &AuthData, device_id: impl AsRef<str>) -> Self {
        Self {
            user_id: auth_data.user_id,
            service_token: auth_data.service_token.to_owned(),
            request_id: format!(
                "app_ios_{}",
                Alphanumeric.sample_string(&mut rand::rng(), 30)
            ),
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

    /// Build a speak operation payload
    pub fn speak(self, text: impl AsRef<str>) -> OpPayload<Speak> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "text_to_speech".to_string(),
                path: "mibrain".to_string(),
                message: Speak {
                    text: text.as_ref().to_string(),
                },
            },
        }
    }

    /// Build a volume setting operation payload
    pub fn volume(self, volume: usize, media: impl AsRef<str>) -> OpPayload<Volume> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "player_set_volume".to_string(),
                path: "mediaplayer".to_string(),
                message: Volume {
                    volume,
                    media: media.as_ref().to_string(),
                },
            },
        }
    }

    /// Build a pause operation payload
    pub fn pause(self, media: impl AsRef<str>) -> OpPayload<Play> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "player_play_operation".to_string(),
                path: "mediaplayer".to_string(),
                message: Play {
                    action: "pause".to_string(),
                    media: media.as_ref().to_string(),
                },
            },
        }
    }

    /// Build a play operation payload (resume play)
    pub fn play(self, media: impl AsRef<str>) -> OpPayload<Play> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "player_play_operation".to_string(),
                path: "mediaplayer".to_string(),
                message: Play {
                    action: "play".to_string(),
                    media: media.as_ref().to_string(),
                },
            },
        }
    }

    /// Build a get play status operation payload
    pub fn status(self, media: impl AsRef<str>) -> OpPayload<Status> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "player_get_play_status".to_string(),
                path: "mediaplayer".to_string(),
                message: Status {
                    media: media.as_ref().to_string(),
                },
            },
        }
    }

    /// Build a play url operation payload
    pub fn play_url(
        self,
        url: impl AsRef<str>,
        r#type: usize,
        media: impl AsRef<str>,
    ) -> OpPayload<PlayUrl> {
        OpPayload {
            user_id: self.user_id,
            service_token: self.service_token,
            request_id: self.request_id,
            device_id: self.device_id,
            op: Op {
                method: "player_play_url".to_string(),
                path: "mediaplayer".to_string(),
                message: PlayUrl {
                    url: url.as_ref().to_string(),
                    r#type,
                    media: media.as_ref().to_string(),
                },
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Speak {
    text: String,
}

#[derive(Debug, Serialize)]
pub struct Volume {
    volume: usize,
    media: String,
}

#[derive(Debug, Serialize)]
pub struct Play {
    action: String,
    media: String,
}

#[derive(Debug, Serialize)]
pub struct Status {
    media: String,
}

#[derive(Debug, Serialize)]
pub struct PlayUrl {
    url: String,
    r#type: usize,
    media: String,
}

#[derive(Debug, Deserialize)]
pub struct OpResponse {
    pub data: OpData,
}

impl Deref for OpResponse {
    type Target = OpData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug, Deserialize)]
pub struct OpData {
    #[serde(deserialize_with = "serde_from_string", default)]
    pub info: Option<Info>,
}

fn serde_from_string<'de, D, T>(deserializer: D) -> core::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let s = String::deserialize(deserializer)?;
    serde_json::from_str(&s).map_err(serde::de::Error::custom)
}

#[derive(Debug, Deserialize)]
pub struct Info {
    pub status: usize,
    pub volume: usize,
    pub loop_type: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::load_or_login_and_save_with_env;
    use crate::device_by_alias;
    use crate::op::{OpApi, OpPayloadBuilder};

    #[tokio::test]
    async fn test_ops() {
        let auth_data = load_or_login_and_save_with_env("auth_data.json")
            .await
            .unwrap();
        let device = device_by_alias(&auth_data, "哈哈").await.unwrap();
        println!("{:#?}", device);
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).status("music");
        println!("{}", serde_json::to_string(&payload).unwrap());
        let resp: OpResponse = OpApi::request(payload).await.unwrap();
        println!("{:#?}", resp);
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).volume(40, "music");
        println!("{}", serde_json::to_string(&payload).unwrap());
        let resp: OpResponse = OpApi::request(payload).await.unwrap();
        println!("{:#?}", resp);
        let payload = OpPayloadBuilder::new(&auth_data, &device.device_id).speak("我是奶龙");
        println!("{}", serde_json::to_string(&payload).unwrap());
        let resp: OpResponse = OpApi::request(payload).await.unwrap();
        println!("{:#?}", resp);
    }
}
