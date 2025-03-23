use api_req::{ApiCaller, Method, Payload, RedirectPolicy, header};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

static DEVICE_ID: LazyLock<String> = LazyLock::new(|| {
    let id = std::env::var("DEVICE_ID")
        .unwrap_or(Alphanumeric.sample_string(&mut rand::rng(), 16))
        .to_uppercase();
    assert_eq!(id.len(), 16, "DEVICE_ID's length must be 16");
    id
});

#[derive(Debug, ApiCaller)]
#[api(
    base_url = "https://account.xiaomi.com",
    default_headers = ((header::USER_AGENT, "APP/com.xiaomi.mihome APPV/6.0.103 iosPassportSDK/3.9.0 iOS/14.4 miHSTS"),),
    redirect = RedirectPolicy::none(),
)]
pub struct AccountApi {}

#[derive(Debug, Serialize, Payload)]
#[payload(
    path = "/pass/serviceLogin?sid=micoapi&_json=true",
    method = Method::GET,
    headers = ((header::COOKIE, format!("sdkVersion=3.9; deviceId={}", &*DEVICE_ID)),),
    req = query,
    before_deserialize = |text: String| text.strip_prefix("&&&START&&&").map(ToOwned::to_owned).ok_or(text),
)]
pub struct LoginPayload {}

#[derive(Debug, Serialize, Deserialize, Payload)]
#[payload(
    path = "/pass/serviceLoginAuth2",
    method = Method::POST,
    headers = ((header::COOKIE, format!("sdkVersion=3.9; deviceId={}", &*DEVICE_ID)),),
    req = form,
    before_deserialize = |text: String| text.strip_prefix("&&&START&&&").map(ToOwned::to_owned).ok_or(text)
)]
pub struct LoginPayload2 {
    #[serde(skip_deserializing)]
    _json: bool,
    qs: String,
    sid: String,
    _sign: String,
    callback: String,
    #[serde(skip_deserializing)]
    user: String,
    #[serde(skip_deserializing)]
    hash: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    #[serde(rename = "userId")]
    pub user_id: Option<i64>,
    #[serde(rename = "passToken")]
    pub pass_token: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    #[serde(flatten)]
    pub payload2: Option<LoginPayload2>,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse2 {
    #[serde(rename = "userId")]
    pub user_id: i64,
    #[serde(rename = "passToken")]
    pub pass_token: String,
    pub location: String,
    pub nonce: i64,
    pub ssecurity: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_login() {
        dotenv::dotenv().ok();
        let payload = LoginPayload {};
        let resp: LoginResponse = match AccountApi::request(payload).await {
            Ok(resp) => resp,
            Err(e) => panic!("{}", e),
        };
        println!("{:#?}", resp);
        let payload2 = LoginPayload2 {
            _json: true,
            user: std::env::var("ACCOUNT_ID").unwrap(),
            hash: {
                hex::encode(md5::compute(std::env::var("ACCOUNT_PASSWORD").unwrap()).iter())
                    .to_uppercase()
            },
            ..resp.payload2.unwrap()
        };
        let resp: LoginResponse2 = match AccountApi::request(payload2).await {
            Ok(resp) => resp,
            Err(e) => panic!("{}", e),
        };
        println!("{:#?}", resp);
    }
}
