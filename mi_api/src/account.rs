use api_req::{ApiCaller, Method, Payload, RedirectPolicy, header};
use base64::{Engine, prelude::BASE64_STANDARD};
use rand::distr::{Alphanumeric, SampleString as _};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::sync::LazyLock;

use crate::sid::Sid;

pub static DEVICE_ID: LazyLock<String> = LazyLock::new(|| {
    let id = std::env::var("DEVICE_ID")
        .unwrap_or(Alphanumeric.sample_string(&mut rand::rng(), 16))
        .to_uppercase();
    assert_eq!(id.len(), 16, "DEVICE_ID's length must be 16");
    id
});

pub async fn login() -> AuthData {
    dotenv::dotenv().ok();
    let payload = LoginPayload {
        device_id: DEVICE_ID.clone(),
        ..Default::default()
    };
    let resp: LoginResponse = match AccountApi::request(payload).await {
        Ok(resp) => resp,
        Err(e) => panic!("{}", e),
    };
    println!("{:#?}", resp);
    if resp.user_id.is_some() {
        return AuthData {
            service_token: resp.service_token().await.unwrap(),
            user_id: resp.user_id.unwrap(),
            divice_id: DEVICE_ID.clone(),
            ssecurity: resp.ssecurity.unwrap(),
        };
    }
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
    AuthData {
        service_token: resp.service_token().await,
        user_id: resp.user_id,
        divice_id: DEVICE_ID.clone(),
        ssecurity: resp.ssecurity,
    }
}

#[derive(Debug)]
pub struct AuthData {
    pub user_id: i64,
    pub divice_id: String,
    pub ssecurity: String,
    pub service_token: String,
}

#[derive(Debug, ApiCaller)]
#[api_req(
    base_url = "https://account.xiaomi.com",
    default_headers = ((header::USER_AGENT, "APP/com.xiaomi.mihome APPV/6.0.103 iosPassportSDK/3.9.0 iOS/14.4 miHSTS"),),
    redirect = RedirectPolicy::none(),
)]
pub struct AccountApi {}

#[derive(Debug, Default, Serialize, Payload)]
#[api_req(
    path = "/pass/serviceLogin?sid={sid}&_json=true",
    method = Method::GET,
    headers = ((header::COOKIE, "sdkVersion=3.9; deviceId={device_id}; userId={user_id}; passToken={pass_token}"),),
    req = query,
    before_deserialize = |text: String| text.strip_prefix("&&&START&&&").map(ToOwned::to_owned).ok_or(text),
)]
pub struct LoginPayload {
    #[serde(skip_serializing)]
    sid: Sid,
    #[serde(skip_serializing)]
    device_id: String,
    #[serde(skip_serializing)]
    user_id: i64,
    #[serde(skip_serializing)]
    pass_token: String,
}

#[derive(Debug, Serialize, Deserialize, Payload)]
#[api_req(
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
    pub nonce: Option<i64>,
    pub ssecurity: Option<String>,
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

async fn service_token(
    location: impl AsRef<str>,
    nonce: i64,
    ssecurity: impl AsRef<str>,
) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("nonce={nonce}&{}", ssecurity.as_ref()));
    let sig = BASE64_STANDARD.encode(hasher.finalize());
    reqwest::get(
        reqwest::Url::parse_with_params(location.as_ref(), &[("clientSign", sig)]).unwrap(),
    )
    .await
    .unwrap()
    .cookies()
    .find(|c| c.name() == "serviceToken")
    .unwrap()
    .value()
    .to_owned()
}

impl LoginResponse {
    pub async fn service_token(&self) -> Result<String, String> {
        Ok(service_token(
            self.location
                .as_deref()
                .ok_or("location not found in resp".to_string())?,
            self.nonce.ok_or(
                "
                nonce not found in resp"
                    .to_string(),
            )?,
            self.ssecurity
                .as_deref()
                .ok_or("ssecurity not found in resp".to_string())?,
        )
        .await)
    }
}
impl LoginResponse2 {
    pub async fn service_token(&self) -> String {
        service_token(&self.location, self.nonce, &self.ssecurity).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_login() {
        dbg!(login().await);
    }
}
