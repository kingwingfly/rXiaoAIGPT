use anyhow::Result;
use api_req::error::ApiErr;
use regex::Regex;
use xiaoai::{
    ApiCaller as _, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder, OpResponse,
    RecordApi, XiaoaiStatus,
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
        let regex3 =
            Regex::new("^(播放|我[想要]听)(?:(?<singer>[^的]+)的)?(?<song>.*).*$").unwrap();
        let regex4 = Regex::new("^(随机播放|(随便)?放一?首歌听{0,2})$").unwrap();
        let mut state = State::On;
        loop {
            tokio::select! {
                _ = async {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let payload = LastAskPayload::new(&self.auth_data, &self.device, 1);
                    if let Ok(resp) = RecordApi::request::<_, LastAskResponse>(payload).await {
                        if let Some(last) = resp.first() {
                            if last.time <= last_ts {
                                return Ok(());
                            }
                            println!("{:?}", last);
                            if regex1.is_match(&last.query) {
                                last_ts = last.time;
                                state = State::On;
                                let _: OpResponse = OpApi::request(
                                    OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，启动！")
                                ).await?;
                            } else if regex2.is_match(&last.query) {
                                last_ts = last.time;
                                state = State::Off;
                                let _: OpResponse = OpApi::request(
                                    OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，关闭！")
                                ).await?;
                            } else if state == State::On {
                                if let Some(capture) = regex3.captures(&last.query) {
                                    last_ts = last.time;
                                    let re = match (capture.name("singer"), capture.name("song") ) {
                                        (Some(singer), Some(song)) => format!(".*{}.*{}.*", singer.as_str(), song.as_str()),
                                        (None, Some(song)) => format!(".*{}.*", song.as_str()),
                                        _ => return Ok(()),
                                    };
                                    println!("Try find regex: {}", re);
                                    let regex = urlencoding::encode(&re).to_string();
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).pause()
                                    ).await?;
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url(format!("http://192.168.1.20:3000/{}", regex))
                                    ).await?;
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        let resp: OpResponse = OpApi::request(
                                            OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).status()
                                        ).await?;
                                        if resp.status() != XiaoaiStatus::Playing {
                                            break;
                                        }
                                    }
                                } else if regex4.is_match(&last.query) {
                                    println!("Try random play");
                                    println!("Play");
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).pause()
                                    ).await?;
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url("http://192.168.1.20:3000/random")
                                    ).await?;
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        let resp: OpResponse = OpApi::request(
                                            OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).status()
                                        ).await?;
                                        if !matches!(resp.status(), XiaoaiStatus::Playing | XiaoaiStatus::Paused ) {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok::<_, ApiErr>(())
                } => {},
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        Ok(())
    }
}
