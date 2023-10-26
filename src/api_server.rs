use log::{ error, info};
use std::{sync::{RwLock, Arc}, thread::{self, JoinHandle}};

use warp::Filter;

use librespot::{
    metadata::audio::AudioItem,
    playback::player::{PlayerEvent, PlayerEventChannel},
};

#[derive(Debug)]
enum PlayingState {
    Playing,
    Paused,
    Stopped
}

#[derive(Debug)]
struct PlayerState {
    audio_item: Option<Box<AudioItem>>,
    playing: PlayingState,
    volume: u16,
}

#[derive(Debug)]
struct ServerState {
    player_state: Arc<RwLock<PlayerState>>,
}


pub struct Server {
    _handle: JoinHandle<()>,
}

impl Server {
    pub fn new(mut player_events: PlayerEventChannel) -> Self{

        info!("Starting api server thread");

        let handle = thread::spawn(move || {
            
            let state = ServerState { 
                player_state: Arc::new(RwLock::new(PlayerState { 
                    audio_item: None,
                    playing: PlayingState::Stopped,
                    volume: 0  
                }))
            };

            let state1 = state.player_state.clone();
            let event_task = Box::pin( async {
                loop{
                    if let Some(e) = player_events.recv().await {
                        handle_internal_event(state1.clone(), e).unwrap();
                    };
                }
            });

            let f = warp::path!("state")
            .and(warp::any().map(move | | state.player_state.clone()))
            .map( move | st: Arc<RwLock<PlayerState>> | {
                info!("New connection");
                let text = st.read().unwrap();
                warp::reply::html(format!("{text:#?}"))
            });

            let http_server = Box::pin(warp::serve(f).run(([0, 0, 0, 0], 3030)));

            let rt = tokio::runtime::Runtime::new().expect("Unable to start server runtime");

            let fut = async { tokio::select! {
                _ = async { http_server.await } => {
                    error!("Http server closed");
                },
                _ = async { event_task.await } => {
                    error!("Event task closed")
                }
            }};

            rt.block_on(fut);
        });

        Self { _handle: handle }

    }
    
}

fn handle_internal_event(state: Arc<RwLock<PlayerState>>, player_event: PlayerEvent) -> Result<(), &'static str>{
        // acquire lock
        let mut state = state.write().expect("Unable to get lock"); 

        match player_event {
            PlayerEvent::Playing { .. } => {
                state.playing = PlayingState::Playing;
            }
            PlayerEvent::Paused { .. } => {
                state.playing = PlayingState::Paused;
                state.audio_item = None;
            }
            PlayerEvent::Stopped { .. } => {
                state.playing = PlayingState::Stopped;
            }
            PlayerEvent::TrackChanged { audio_item } => {
                state.audio_item = Some(audio_item);
            }
            PlayerEvent::VolumeChanged { volume } => {
                state.volume = volume;
            }
            _ => {},
        }
        
        Ok(())
    }