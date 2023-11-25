use bytes::Bytes;
use log::{debug, info};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    str,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
};

use futures_util::{SinkExt, StreamExt};
use static_dir::static_dir;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use warp::{ws, Filter};

use crate::json_result::{JsonError, JsonResponse, JsonResult};

use librespot_connect::spirc::SpircCommand;
use librespot_metadata::{audio::AudioItem, audio::UniqueFields};
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};

static UID_NEXT: AtomicUsize = AtomicUsize::new(1);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct JsonRequest {
    id: i64,
    jsonrpc: f32,
    method: String,
    params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
enum Notification {
    Play,
    Pause,
    Stop,
    NewTrack(Track),
    VolumeChange(u16),
    Shuffle(bool),
}

#[derive(Debug, Serialize, Clone)]
struct JsonNotification {
    jsonrpc: f32,
    method: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
enum PlayingState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Serialize, Clone)]
struct Cover {
    url: String,
    size: (i32, i32),
}

#[derive(Debug, Serialize, Clone)]
struct Track {
    track_id: String,
    name: String,
    covers: Vec<Cover>,
    album: Option<String>,
    artists: Vec<String>,
    show_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct PlayerState {
    track: Option<Track>,
    playing: PlayingState,
    volume: u16,
    shuffle: bool,
}

type UserTaskVec = Arc<RwLock<HashMap<usize, tokio::task::JoinHandle<()>>>>;

struct ServerInternal {
    player_state: Arc<RwLock<PlayerState>>,
    user_tasks: UserTaskVec,
    user_message_tx: broadcast::Sender<JsonNotification>,
    rt: tokio::runtime::Handle,
    cancel: CancellationToken,
    spirc: Arc<RwLock<Option<mpsc::UnboundedSender<SpircCommand>>>>,
}

pub struct Server {
    handle: thread::JoinHandle<()>,
    internal: Arc<ServerInternal>,
}

impl Server {
    pub fn new(
        mut player_events: PlayerEventChannel,
        enable_web: bool,
        custom_path: Option<String>,
    ) -> Self {
        info!("Starting api server thread");

        let rt = tokio::runtime::Runtime::new().expect("Unable to start server runtime");

        let cancel = CancellationToken::new();

        let (pub_tx, _) = broadcast::channel::<JsonNotification>(16);

        let state = Arc::new(ServerInternal {
            player_state: Arc::new(RwLock::new(PlayerState {
                track: None,
                playing: PlayingState::Stopped,
                volume: 0,
                shuffle: false,
            })),
            user_tasks: Arc::new(RwLock::new(HashMap::new())),
            user_message_tx: pub_tx,
            rt: rt.handle().clone(),
            cancel,
            spirc: Arc::new(RwLock::new(None)),
        });

        let state1 = state.clone();

        let handle = thread::spawn(move || {
            let state2 = state1.clone();
            let _event_task = rt.spawn(async move {
                loop {
                    if let Some(e) = player_events.recv().await {
                        state2.handle_internal_event(e);
                    };
                }
            });

            let state2 = state1.clone();
            let with_state = warp::any().map(move || state2.clone().to_owned());

            let ws_path = warp::path::end().and(ws()).and(with_state.clone()).map(
                |ws: ws::Ws, state2: Arc<ServerInternal>| {
                    debug!("New websocket connection");
                    ws.on_upgrade(|sock| async move { state2.add_user(sock) })
                },
            );

            let post_path = warp::path::end()
                .and(warp::post())
                .and(warp::body::bytes())
                .and(with_state.clone())
                .map(|body: Bytes, state2: Arc<ServerInternal>| {
                    debug!("New http POST request");
                    let req: &str = str::from_utf8(body.as_ref()).unwrap();
                    match state2.handle_request(req) {
                        Ok(res) => {
                            serde_json::to_string(&res).expect("Unable to serialize response")
                        }
                        Err(err) => {
                            serde_json::to_string(&err).expect("Unable to serialize error response")
                        }
                    }
                });

            let custom_dir = custom_path.is_some();

            let dir = match custom_path {
                Some(s) => s,
                None => "".to_string(),
            };

            let get_path_custom = warp::any()
                .and_then(move || async move {
                    if enable_web & custom_dir {
                        Ok(())
                    } else {
                        Err(warp::reject::not_found())
                    }
                })
                .and(warp::fs::dir(dir))
                .map(|_, d| d);

            let get_path_static = warp::any()
                .and_then(move || async move {
                    if enable_web & !custom_dir {
                        Ok(())
                    } else {
                        Err(warp::reject::not_found())
                    }
                })
                .and(static_dir!("./static"))
                .map(|_, d| d);

            let path = post_path
                .or(ws_path)
                .or(get_path_custom)
                .or(get_path_static);

            let http_server = Box::pin(warp::serve(path).run(([0, 0, 0, 0], 3030)));

            rt.block_on(async {
                    tokio::select! {
                    _ = http_server => {},
                    _ = state1.cancel.cancelled() => {},
                }
            });

            info!("Shutting down API server")
        });

        Self {
            handle,
            internal: state,
        }
    }

    pub fn set_spirc_channel(&self, spirc: mpsc::UnboundedSender<SpircCommand>) {
        debug!("Spirc command channel set");
        let mut channel = self.internal.spirc.write();
        *channel = Some(spirc);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.internal.cancel.cancel();
    }
}

impl ServerInternal {
    fn handle_internal_event(&self, player_event: PlayerEvent) {
        let mut notif: Option<Notification> = None;
        debug!("Recieved PlayerEvent: {player_event:?}");

        {
            // Needs to drop lock before sending notification,
            // otherwise forward_event will wait forever to lock the same variable
            let mut state = self.player_state.write();

            match player_event {
                PlayerEvent::Playing { .. } => {
                    state.playing = PlayingState::Playing;
                    notif = Some(Notification::Play);
                }
                PlayerEvent::Paused { .. } => {
                    state.playing = PlayingState::Paused;
                    notif = Some(Notification::Pause);
                }
                PlayerEvent::Stopped { .. } => {
                    state.playing = PlayingState::Stopped;
                    state.track = None;
                    notif = Some(Notification::Stop);
                }
                PlayerEvent::TrackChanged { audio_item } => {
                    let track = Track::from_audio_item(*audio_item);
                    state.track = Some(track.clone());
                    debug!("New track recieved: {track:?}");
                    notif = Some(Notification::NewTrack(track));
                }
                PlayerEvent::VolumeChanged { volume } => {
                    state.volume = volume;
                    notif = Some(Notification::VolumeChange(volume));
                }
                PlayerEvent::ShuffleChanged { shuffle } => {
                    state.shuffle = shuffle;
                    notif = Some(Notification::Shuffle(shuffle));
                }
                _ => {}
            }
        }

        if let Some(n) = notif {
            self.forward_event(n);
        }
    }

    fn forward_event(&self, event: Notification) {
        if self.user_message_tx.receiver_count() != 0 {
            debug!("Sending notification to connected websockets");
            let m = match event {
                Notification::NewTrack(track) => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnNewTrack".to_string(),
                    params: json!({"track": track}),
                },
                Notification::Pause => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPause".to_string(),
                    params: serde_json::Value::Null,
                },
                Notification::Play => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPlay".to_string(),
                    params: serde_json::Value::Null,
                },
                Notification::Stop => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnStop".to_string(),
                    params: serde_json::Value::Null,
                },
                Notification::VolumeChange(vol) => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnVolumeChange".to_string(),
                    params: json!({"volume": vol}),
                },
                Notification::Shuffle(shuffle) => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnShuffleChange".to_string(),
                    params: json!({"shuffle": shuffle}),
                },
            };

            // Errors if last reciever dropped since check,
            // unlikely and can be ignored.
            let _ = self.user_message_tx.send(m);
        }
    }

    fn add_user(self: Arc<Self>, sock: warp::ws::WebSocket) {
        let mut event_channel = self.user_message_tx.subscribe();

        let uid = UID_NEXT.fetch_add(1, Ordering::Relaxed);
        let num_open = self.user_message_tx.receiver_count();
        debug!("Adding new websocket connection, ID: {uid}, currently open: {num_open}");

        let users = self.user_tasks.clone();
        let state = self.clone();
        let cancel = self.cancel.clone();

        let thr = self.rt.spawn(async move {
            let (mut tx, mut ws_rx) = sock.split();

            // socket JSONs command -> player command -> socket response
            // internal event JSONs -> socket notification

            loop {

                let data: String = tokio::select! {
                    message = ws_rx.next() => {
                        debug!("New request from WS ID: {uid}");
                        match message {
                            None => break,
                            Some(m) => {

                                match &m {
                                    Ok(ms) => if ms.is_close() {debug!("Got close from WS ID: {uid}"); break;},
                                    Err(_) => ()
                                }

                                let res = state.handle_socket_message(m);
                                match res {
                                    Ok(res) => serde_json::to_string(&res).expect("Should be able to parse result"),
                                    Err(e) => serde_json::to_string(&e).expect("Should be able to parse error"),
                                }
                            },
                        }
                    },
                    event = event_channel.recv() => {
                        debug!("New event to WS ID: {uid}");
                        match event {
                            Ok(m) => {
                                serde_json::to_string(&m).expect("Should be able to parse notification")
                            },
                            Err(e) => format!("Internal server error: {e}").to_string(),
                        }
                    }
                    _ = cancel.cancelled() => {
                        // We don't care about result since we are shutting down
                        let _ = tx.send(ws::Message::close()).await;
                        break;
                    }
                };

                match tx.send(ws::Message::text(data)).await {
                    Ok(_) => (),
                    Err(e) => {debug!("{e}"); break;}
                };
            };

            debug!("dropping websocket id {uid}");
            users.write().remove(&uid);
        });

        self.user_tasks.write().insert(uid, thr);
    }

    fn handle_socket_message(&self, message: Result<ws::Message, warp::Error>) -> JsonResult {
        let m = message.map_err(|e| JsonError::internal(Some(e.to_string())))?;

        let m = m
            .to_str()
            .map_err(|_| JsonError::invalid_request(Some("Malformed data".to_string())))?;

        self.handle_request(m)
    }

    fn handle_request(&self, request: &str) -> JsonResult {
        let val: serde_json::Value = serde_json::from_str(request)?;
        let id = match &val["id"] {
            serde_json::Value::Number(n) => n,
            serde_json::Value::Null => {
                return Err(JsonError::invalid_request(Some(
                    "No id field found".to_string(),
                )))
            }
            _ => return Err(JsonError::parse(Some("Unexpected id value".to_string()))),
        };

        let id = match id.as_i64() {
            Some(v) => v,
            None => {
                return Err(JsonError::invalid_request(Some(
                    "Invalid id value".to_string(),
                )))
            }
        };

        let mut res = self.do_request(val);

        match res.as_mut() {
            Ok(resp) => resp.set_id(id),
            Err(e) => e.set_id(Some(id)),
        };

        res
    }

    fn do_request(&self, req: serde_json::Value) -> JsonResult {
        let req: JsonRequest = serde_json::from_value(req)?;

        let result: serde_json::Value = match req.method.as_str() {
            "getStatus" => json!(self.player_state.as_ref()),
            "getVolume" => json!({"volume": self.player_state.read().volume}),
            "getPlayState" => json!({"playing": &self.player_state.read().playing}),
            "setPlay" => json!(self.send_command(SpircCommand::Play)?),
            "setPause" => json!(self.send_command(SpircCommand::Pause)?),
            "setNext" => json!(self.send_command(SpircCommand::Next)?),
            "setShuffleOn" => json!(self.send_command(SpircCommand::Shuffle(true))?),
            "setShuffleOff" => json!(self.send_command(SpircCommand::Shuffle(false))?),
            "setVolume" => {
                let vol = req.params;
                let vol = match vol {
                    Some(serde_json::Value::Number(v)) => v.as_u64().ok_or_else(|| {
                        JsonError::invalid_param(Some("Volume not a number".to_string()))
                    })? as u16,
                    _ => {
                        return Err(JsonError::invalid_param(Some(
                            "Volume not a number".to_string(),
                        )))
                    }
                };

                json!(self.send_command(SpircCommand::SetVolume(vol))?)
            }
            _ => return Err(JsonError::method_not_found(None)),
        };

        Ok(JsonResponse::new(req.id, result))
    }

    fn send_command(&self, command: SpircCommand) -> Result<String, JsonError> {
        let sp = self.spirc.read();
        debug!("Sending spirc command: {command:?}");

        match *sp {
            Some(ref sp) => {
                sp.send(command)
                    .map_err(|e| JsonError::internal(Some(e.to_string())))?;
                Ok("Ok".to_string())
            }
            None => Err(JsonError::no_control(None)),
        }
    }
}

impl Track {
    fn from_audio_item(item: AudioItem) -> Self {
        let covers = item
            .covers
            .into_iter()
            .map(|c| Cover {
                url: c.url,
                size: (c.width, c.height),
            })
            .collect();

        let (album, artists, show_name) = match item.unique_fields {
            UniqueFields::Track { artists, album, .. } => (
                Some(album),
                artists.0.into_iter().map(|a| a.name).collect(),
                None,
            ),
            UniqueFields::Episode { show_name, .. } => (None, Vec::new(), Some(show_name)),
        };

        Track {
            track_id: item.track_id.to_base62().unwrap(),
            name: item.name,
            covers,
            album,
            artists,
            show_name,
        }
    }
}
