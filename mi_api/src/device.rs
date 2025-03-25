use api_req::{ApiCaller, Method, Payload, header};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use crate::login;

    use super::*;

    #[tokio::test]
    async fn list_test() {
        let auth_data = login().await;
        println!("{:#?}", auth_data);
        let payload = DeviceListPayload {
            user_id: auth_data.user_id,
            service_token: auth_data.service_token,
            master: 0,
            request_id: format!(
                "app_ios_{}",
                Alphanumeric.sample_string(&mut rand::rng(), 30)
            ),
        };
        println!("{}", serde_json::to_string(&payload).unwrap());
        let resp: serde_json::Value = DeviceApi::request(payload).await.unwrap();
        println!("{:#?}", resp);
    }
}
