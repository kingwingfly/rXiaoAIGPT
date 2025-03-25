use api_req::{ApiCaller, Method, Payload, header};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize};

use crate::AuthData;

pub async fn device_by_alias(auth_data: &AuthData, alias: impl AsRef<str>) -> Device {
    let payload = DeviceListPayload {
        user_id: auth_data.user_id,
        service_token: auth_data.service_token.to_owned(),
        ..Default::default()
    };
    println!("{}", serde_json::to_string(&payload).unwrap());
    let resp: DeviceListResponse = DeviceApi::request(payload).await.unwrap();
    println!("{:#?}", resp);
    resp.data
        .into_iter()
        .find(|d| d.alias == alias.as_ref())
        .unwrap_or_else(|| panic!("device alias not found"))
}

#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = "https://api2.mina.mi.com",
    default_headers = (
        (header::USER_AGENT, "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103"),
    )
)]
pub struct DeviceApi {}

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

impl Default for DeviceListPayload {
    fn default() -> Self {
        Self {
            user_id: 0,
            service_token: String::new(),
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

#[derive(Debug, Deserialize)]
pub struct Device {
    pub alias: String,
    #[serde(rename = "deviceID")]
    pub device_id: String,
    #[serde(flatten)]
    pub others: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use crate::load_or_login_and_save;

    use super::*;

    #[tokio::test]
    async fn list_test() {
        let auth_data = load_or_login_and_save("auth_data.json").await;
        let device = device_by_alias(&auth_data, "哈哈").await;
        println!("{:#?}", device);
    }
}
