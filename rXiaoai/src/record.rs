use api_req::{ApiCaller, Method, Payload, header};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

use crate::{account::AuthData, op::Device, serde_util};

/// Reads the speaker's conversation history.
#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = "https://userprofile.mina.mi.com",
    default_headers = [(header::USER_AGENT, "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103")],
)]
pub struct RecordApi {}

/// Query for the most recent conversation records.
#[derive(Debug, Serialize, Payload)]
#[api_req(
    path = "/device_profile/v2/conversation?source=dialogu",
    method = Method::GET,
    headers = [(header::COOKIE, "deviceId={device_id}; serviceToken={service_token}; userId={user_id}")],
    req = query
)]
pub struct LastAskPayload {
    #[serde(skip_serializing)]
    user_id: i64,
    #[serde(skip_serializing)]
    service_token: String,
    device_id: String,
    hardware: String,
    timestamp: usize,
    limit: usize,
}

impl LastAskPayload {
    /// `limit` is how many records to fetch.
    pub fn new(auth_data: &AuthData, device: &Device, limit: usize) -> Self {
        Self {
            user_id: auth_data.user_id,
            service_token: auth_data.service_token.to_owned(),
            device_id: device.device_id.to_owned(),
            hardware: device.hardware.to_owned(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as usize,
            limit,
        }
    }
}

/// Derefs to [`Data`], and through it to `[Record]`.
#[derive(Debug, Deserialize)]
pub struct LastAskResponse {
    #[serde(deserialize_with = "serde_util::from_string")]
    pub data: Data,
}

impl Deref for LastAskResponse {
    type Target = Data;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// Derefs to `[Record]`.
#[derive(Debug, Deserialize)]
pub struct Data {
    pub records: Vec<Record>,
    #[serde(rename = "nextEndTime")]
    pub next_end_time: usize,
}

impl Deref for Data {
    type Target = Vec<Record>;

    fn deref(&self) -> &Self::Target {
        &self.records
    }
}

#[derive(Debug, Deserialize)]
pub struct Record {
    pub answers: Vec<Answer>,
    pub time: usize,
    pub query: String,
    #[serde(rename = "requestId")]
    pub request_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Answer {
    #[serde(rename = "type")]
    pub answer_type: String,
    #[serde(default)]
    pub tts: Option<Tts>,
    #[serde(default)]
    pub audio: Option<Audio>,
}

#[derive(Debug, Deserialize)]
pub struct Tts {
    pub text: String,
}

/// Derefs to `[AudioInfo]`.
#[derive(Debug, Deserialize)]
pub struct Audio {
    #[serde(rename = "audioInfoList")]
    pub audio_info_list: Vec<AudioInfo>,
}

impl Deref for Audio {
    type Target = Vec<AudioInfo>;

    fn deref(&self) -> &Self::Target {
        &self.audio_info_list
    }
}

#[derive(Debug, Deserialize)]
pub struct AudioInfo {
    pub title: String,
    pub artist: String,
    #[serde(rename = "cpName")]
    pub cp_name: String,
}

#[cfg(test)]
mod tests {
    use crate::{account::load_or_login_and_save_with_env, device_by_alias};

    use super::*;

    #[tokio::test]
    async fn last_ask_test() {
        let auth_data = load_or_login_and_save_with_env(crate::AUTH_DATA_PATH)
            .await
            .unwrap();
        let device = device_by_alias(&auth_data, "哈哈").await.unwrap();
        let payload = LastAskPayload::new(&auth_data, &device, 2);
        let _: LastAskResponse = RecordApi::request(payload).await.unwrap();
    }
}
