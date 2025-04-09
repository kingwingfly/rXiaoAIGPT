use anyhow::Result;
use axum::{
    Router,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Redirect, Response},
    routing::get,
};
use mime_guess::MimeGuess;
use rand::seq::IteratorRandom as _;
use regex::Regex;
use std::{collections::HashSet, sync::Arc};
use tokio::{net::TcpListener, sync::RwLock};
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use xiaoai::{
    ApiCaller as _, ApiErr, Device, LastAskPayload, LastAskResponse, OpApi, OpPayloadBuilder,
    OpResponse, RecordApi, XiaoaiStatus,
    account::{AuthData, load_or_login_and_save_with_env},
    device_by_alias,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentState {
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

    pub async fn run(&self, ip: impl AsRef<str>, port: u16) -> Result<()> {
        let url = format!("http://{}:{}", ip.as_ref(), port);
        println!("{}", url);
        tokio::spawn(async move {
            let music = Arc::new(RwLock::new(HashSet::<String>::new()));
            let app = Router::new()
                .fallback_service(ServeDir::new("."))
                .layer(ServiceBuilder::new().layer(from_fn_with_state(music.clone(), find_file)))
                .route("/random", get(random_music))
                .route("/random/{singer}", get(random_music_of))
                .with_state(music);
            let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
                .await
                .unwrap();
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });

        let mut last_ts = 0;
        let regex1 = Regex::new("^嘻嘻.*").unwrap();
        let regex2 = Regex::new("^不嘻嘻.*").unwrap();
        let regex3 = Regex::new("^(播放|我[想要]听)(?<singer>[^的]+)的歌$").unwrap();
        let regex4 =
            Regex::new("^(播放|我[想要]听)(?:(?<singer>[^的]+)的)?(?<song>.*).*$").unwrap();
        let regex5 = Regex::new("^(随机播放|(随便)?放一?首歌听{0,2})$").unwrap();
        let mut state = AgentState::On;
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
                                state = AgentState::On;
                                let _: OpResponse = OpApi::request(
                                    OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，启动！")
                                ).await?;
                            } else if regex2.is_match(&last.query) {
                                last_ts = last.time;
                                state = AgentState::Off;
                                let _: OpResponse = OpApi::request(
                                    OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).speak("奶龙，关闭！")
                                ).await?;
                            } else if state == AgentState::On {
                                if let Some(capture) = regex3.captures(&last.query) {
                                    if let Some(singer) = capture.name("singer") {
                                        println!("Try random play {}", singer.as_str());
                                        let _: OpResponse = OpApi::request(
                                            OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).pause()
                                        ).await?;
                                        let _: OpResponse = OpApi::request(
                                            OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url(format!("{}/random/{}", url, singer.as_str()))
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
                                } else if let Some(capture) = regex4.captures(&last.query) {
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
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url(format!("{}/{}", url, regex))
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
                                } else if regex5.is_match(&last.query) {
                                    println!("Try random play");
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).pause()
                                    ).await?;
                                    let _: OpResponse = OpApi::request(
                                        OpPayloadBuilder::new(&self.auth_data, &self.device.device_id).play_url(format!("{}/random", url))
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

#[cfg_attr(debug_assertions, axum::debug_middleware)]
async fn find_file(
    State(state): State<Arc<RwLock<HashSet<String>>>>,
    mut req: Request,
    next: Next,
) -> Response {
    let uri = req.uri().path();
    let regex = urlencoding::decode(uri.strip_prefix('/').unwrap_or(uri)).unwrap();
    println!("regex: {}", regex);
    if regex.len() > 64 {
        return (StatusCode::BAD_REQUEST, "Too long").into_response();
    }
    let re = Regex::new(&regex).unwrap();
    {
        let state = state.read().await;
        for entry in state.iter() {
            if re.is_match(entry) {
                let uri = format!("/{}", urlencoding::encode(entry)).parse().unwrap();
                *req.uri_mut() = uri;
                drop(state);
                return next.run(req).await;
            }
        }
    }
    let mut state = state.write().await;
    for entry in walkdir::WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let mime = MimeGuess::from_path(entry.path()).first_or_octet_stream();
        if mime.type_() != "audio" {
            continue;
        }
        let path = entry.path().to_str().unwrap().trim_matches(['.', '/']);
        state.insert(path.to_string());
        if re.is_match(path) {
            let uri = format!("/{}", urlencoding::encode(path)).parse().unwrap();
            *req.uri_mut() = uri;
            drop(state);
            return next.run(req).await;
        }
    }
    (StatusCode::NOT_FOUND, "Music not match").into_response()
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random_music(State(state): State<Arc<RwLock<HashSet<String>>>>) -> Response {
    {
        let mut state = state.write().await;
        for entry in walkdir::WalkDir::new(".")
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let mime = MimeGuess::from_path(entry.path()).first_or_octet_stream();
            if mime.type_() != "audio" {
                continue;
            }
            let path = entry.path().to_str().unwrap().trim_matches(['.', '/']);
            state.insert(path.to_string());
        }
    }
    {
        let state = state.read().await;
        if let Some(entry) = state.iter().choose(&mut rand::rng()) {
            return Redirect::to(&format!("/{}", urlencoding::encode(entry))).into_response();
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, "Failed to find music").into_response()
}

#[cfg_attr(debug_assertions, axum::debug_handler)]
async fn random_music_of(
    State(state): State<Arc<RwLock<HashSet<String>>>>,
    Path(singer): Path<String>,
) -> Response {
    {
        let mut state = state.write().await;
        for entry in walkdir::WalkDir::new(".")
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let mime = MimeGuess::from_path(entry.path()).first_or_octet_stream();
            if mime.type_() != "audio" {
                continue;
            }
            let path = entry.path().to_str().unwrap().trim_matches(['.', '/']);
            state.insert(path.to_string());
        }
    }
    {
        let re = Regex::new(&format!(".*{}.*", singer)).unwrap();
        let state = state.read().await;
        if let Some(entry) = state
            .iter()
            .filter(|name| re.is_match(name))
            .choose(&mut rand::rng())
        {
            return Redirect::to(&format!("/{}", urlencoding::encode(entry))).into_response();
        }
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to find music of the singer",
    )
        .into_response()
}
