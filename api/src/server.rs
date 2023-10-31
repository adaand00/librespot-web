use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
};

use parking_lot::RwLock;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use warp::Filter;

use crate::json_result::{JsonError, JsonResponse, JsonResult};

use librespot_connect::spirc::SpircCommand;
use librespot_metadata::{audio::AudioItem, audio::UniqueFields};
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};

static UID_NEXT: AtomicUsize = AtomicUsize::new(1);

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
}

#[derive(Debug, Serialize)]
struct JsonNotification {
    jsonrpc: f32,
    method: String,
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
    track_id: u128,
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
}

type UserTaskVec = Arc<RwLock<HashMap<usize, tokio::task::JoinHandle<()>>>>;

struct ServerInternal {
    player_state: Arc<RwLock<PlayerState>>,
    user_tasks: UserTaskVec,
    user_message_tx: broadcast::Sender<String>,
    rt: tokio::runtime::Handle,
    spirc: Arc<RwLock<Option<mpsc::UnboundedSender<SpircCommand>>>>,
}

pub struct Server {
    handle: thread::JoinHandle<()>,
    internal: Arc<ServerInternal>,
}

impl Server {
    pub fn new(mut player_events: PlayerEventChannel) -> Self {
        info!("Starting api server thread");

        let rt = tokio::runtime::Runtime::new().expect("Unable to start server runtime");

        let (pub_tx, _) = broadcast::channel::<String>(16);

        let state = Arc::new(ServerInternal {
            player_state: Arc::new(RwLock::new(PlayerState {
                track: None,
                playing: PlayingState::Stopped,
                volume: 0,
            })),
            user_tasks: Arc::new(RwLock::new(HashMap::new())),
            user_message_tx: pub_tx,
            rt: rt.handle().clone(),
            spirc: Arc::new(RwLock::new(None)),
        });

        let state1 = state.clone();

        let handle = thread::spawn(move || {
            let state2 = state1.clone();
            let _event_task = rt.spawn(async move {
                loop {
                    if let Some(e) = player_events.recv().await {
                        state2.handle_internal_event(e).unwrap();
                    };
                }
            });

            let state2 = state1.clone();
            let with_state = warp::any().map(move || state2.clone().to_owned());

            let ws_path = warp::path::end()
                .and(warp::ws())
                .and(with_state.clone())
                .map(|ws: warp::ws::Ws, state2: Arc<ServerInternal>| {
                    debug!("New websocket connection");
                    ws.on_upgrade(|sock| async move { state2.add_user(sock) })
                });

            let f = warp::path::end()
                .and(warp::post())
                .and(warp::body::json())
                .and(with_state.clone())
                .map(
                    |body: serde_json::Value, state2: Arc<ServerInternal>| {
                        debug!("New http POST request");
                        match state2.handle_request(body) {
                            Ok(res) => serde_json::to_string(&res).expect("Unable to serialize response"),
                            Err(err) => serde_json::to_string(&err).expect("Unable to serialize error response"),
                        }
                    },
                );

            let path = f.or(ws_path);

            let http_server = Box::pin(warp::serve(path).run(([0, 0, 0, 0], 3030)));

            rt.block_on(http_server);
            error!("Http server closed, shutting down API server")
        });

        Self {
            handle,
            internal: state,
        }
    }

    pub fn set_spirc_channel(&self, spirc: mpsc::UnboundedSender<SpircCommand>) {
        let mut channel = self.internal.spirc.write();
        *channel = Some(spirc);
    }
}

impl ServerInternal {
    fn handle_internal_event(&self, player_event: PlayerEvent) -> Result<(), JsonError> {
        let mut notif: Option<Notification> = None;

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
                    notif = Some(Notification::NewTrack(track));
                }
                PlayerEvent::VolumeChanged { volume } => {
                    state.volume = volume;
                    notif = Some(Notification::VolumeChange(volume.clone()));
                }
                _ => {}
            }
        }

        if let Some(n) = notif {
            self.forward_event(n)?;
        }

        Ok(())
    }

    fn forward_event(&self, event: Notification) -> Result<(), JsonError> {
        if self.user_message_tx.receiver_count() != 0 {
            let m = match event {
                Notification::NewTrack(track) => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnNewTrack".to_string(),
                    params: serde_json::to_value(track)?,
                },
                Notification::Pause => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPause".to_string(),
                    params: serde_json::Value::Array(Vec::new()),
                },
                Notification::Play => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPlay".to_string(),
                    params: serde_json::to_value(&self.player_state.read().track)?,
                },
                Notification::Stop => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPause".to_string(),
                    params: serde_json::Value::Array(Vec::new()),
                },
                Notification::VolumeChange(vol) => JsonNotification {
                    jsonrpc: 2.0,
                    method: "OnPlay".to_string(),
                    params: serde_json::to_value(vol)?,
                },
            };

            self.user_message_tx.send(serde_json::to_string(&m)?)?;
        }
        Ok(())
    }

    fn add_user(&self, sock: warp::ws::WebSocket) {
        let mut event_channel = self.user_message_tx.subscribe();

        let uid = UID_NEXT.fetch_add(1, Ordering::Relaxed);

        let users = self.user_tasks.clone();

        let thr = self.rt.spawn(async move {
            let (mut tx, mut rx) = sock.split();

            // socket JSONs command -> player command -> socket response
            let sock_listener = async move {
                loop {
                    let m = rx.next().await;
                    match m {
                        Some(Ok(val)) => {
                            info!("Got {val:?} from WebSocket")
                        }
                        Some(Err(err)) => {
                            error!("Got {err:?} from Websocket")
                        }
                        None => break,
                    };
                }
            };

            // internal event JSONs -> socket
            let sock_writer = async move {
                loop {
                    let ev = event_channel.recv().await;
                    match ev {
                        Ok(m) => tx
                            .send(warp::ws::Message::text(m))
                            .await
                            .expect("Internal event broadcast stopped"),
                        Err(_) => break,
                    };
                }
            };

            tokio::select! {
                _ = sock_listener => {},
                _ = sock_writer => {},
            }

            debug!("dropping websocket");
            users.write().remove(&uid);
        });

        self.user_tasks.write().insert(uid, thr);
    }

    fn handle_request(&self, request: serde_json::Value) -> JsonResult {
        if let serde_json::Value::Number(_n) = &request["id"] {
            return Err(JsonError::invalid_request(Some("No ID".to_string())));
        }

        let req: JsonRequest = serde_json::from_value(request)?;

        let result = match req.method.as_str() {
            "getStatus" => serde_json::to_value(self.player_state.as_ref())?,
            "getVolume" => serde_json::to_value(self.player_state.read().volume)?,
            "getPlayState" => serde_json::to_value(&self.player_state.read().playing)?,
            "setPlay" => serde_json::to_value(self.send_command(SpircCommand::Play)?)?,
            "setPause" => serde_json::to_value(self.send_command(SpircCommand::Pause)?)?,
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

                let res = self.send_command(SpircCommand::SetVolume(vol))?;
                serde_json::to_value(res)?
            }
            _ => return Err(JsonError::method_not_found(None)),
        };

        Ok(JsonResponse::new(req.id, result))
    }

    fn send_command(&self, command: SpircCommand) -> Result<(), JsonError> {
        let sp = self.spirc.read();

        match *sp {
            Some(ref sp) => {
                sp.send(command)
                    .or_else(|e| Err(JsonError::internal(Some(e.to_string()))))?;
                Ok(())
            }
            None => Err(JsonError::no_control(None)),
        }
    }
}

// impl JsonResponse {
//     fn new_e(id: Option<u64>, code: JsonErrCode) -> Self {
//         let message = match code {
//             JsonErrCode::Parse => "Parse error",
//             JsonErrCode::InvalidReq => "Invalid Request",
//             JsonErrCode::MethodNotFound => "Method not found",
//             JsonErrCode::InvalidParam => "Invalid params",
//             JsonErrCode::Internal => "Internal jsonrpc error",
//             JsonErrCode::NoStream => "Internal error (No control channel)",
//             JsonErrCode::SpircPoison => "Internal error (Spirc poisoned)",
//             JsonErrCode::PlayerPoison => "Internal error (Player state poisoned)",
//         };

//         JsonResponse::JsonError {
//             id,
//             jsonrpc: 2.0,
//             error: JsonErrorStruct {
//                 code,
//                 message: message.to_string(),
//             },
//         }
//     }
// }

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
            track_id: item.track_id.id,
            name: item.name,
            covers,
            album,
            artists,
            show_name,
        }
    }
}
