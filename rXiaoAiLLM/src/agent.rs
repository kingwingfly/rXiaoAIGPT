use anyhow::Result;
use regex::Regex;
use xiaoai::{
    ApiCaller as _, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder, OpResponse,
    RecordApi,
    account::{AuthData, load_or_login_and_save_with_env},
    device_by_alias,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    On,
    Off,
}

#[derive(Debug)]
pub struct Agent {
    auth_data: AuthData,
    device: Device,
}

impl Agent {
    pub async fn new(device_alias: impl AsRef<str>) -> Result<Self> {
        let auth_data = load_or_login_and_save_with_env("auth_data.json").await?;
        let device = device_by_alias(&auth_data, device_alias).await?;
        Ok(Self { auth_data, device })
    }

    pub async fn run(&self) -> Result<()> {
        let mut last_ts = 0;
        let regex1 = Regex::new("^嘻嘻.*").unwrap();
        let regex2 = Regex::new("^不嘻嘻.*").unwrap();
        let regex3 = Regex::new("^(播放|我要听)(?<singer>.*?)的?(?<song>.*).*$").unwrap();
        let mut state = State::On;
        loop {
            let payload = LastAskPayload::new(&self.auth_data, &self.device, 1);
            tokio::select! {
                Ok(resp) = RecordApi::request::<_, LastAskResponse>(payload) => {
                    if let Some(last) = resp.first() {
                        if last.time <= last_ts {
                            continue;
                        }
                        last_ts = last.time;
                        println!("{:?}", last);
                        if regex1.is_match(&last.query) {
                            state = State::On;
                            let _: OpResponse = OpApi::request(
                                OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，启动！")
                            ).await?;
                        } else if regex2.is_match(&last.query) {
                            state = State::Off;
                            let _: OpResponse = OpApi::request(
                                OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，关闭！")
                            ).await?;
                        } else if state == State::On {
                            let capture = regex3.captures(&last.query).unwrap();
                            let singer = capture.name("singer").unwrap().as_str();
                            let song = capture.name("song").unwrap().as_str();
                            let regex = urlencoding::encode(&format!(".*{}.*{}.*", singer, song)).to_string();
                            let _: OpResponse = OpApi::request(
                                OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url(format!("http://192.168.1.20:3000/{}", regex), 1, "music")
                            ).await?;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        Ok(())
    }
}
