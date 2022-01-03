use std::{
    cmp::max,
    collections::HashMap,
    fmt,
    future::Future,
    io::{self, Read, Seek, SeekFrom},
    mem,
    pin::Pin,
    process::exit,
    sync::Arc,
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};

use byteorder::{LittleEndian, ReadBytesExt};
use futures_util::{future, stream::futures_unordered::FuturesUnordered, StreamExt, TryFutureExt};
use parking_lot::Mutex;
use symphonia::core::io::MediaSource;
use tokio::sync::{mpsc, oneshot};

use crate::{
    audio::{
        AudioDecrypt, AudioFile, StreamLoaderController, READ_AHEAD_BEFORE_PLAYBACK,
        READ_AHEAD_BEFORE_PLAYBACK_ROUNDTRIPS, READ_AHEAD_DURING_PLAYBACK,
        READ_AHEAD_DURING_PLAYBACK_ROUNDTRIPS,
    },
    audio_backend::Sink,
    config::{Bitrate, NormalisationMethod, NormalisationType, PlayerConfig},
    convert::Converter,
    core::{util::SeqGenerator, Error, Session, SpotifyId},
    decoder::{AudioDecoder, AudioPacket, PassthroughDecoder, SymphoniaDecoder},
    metadata::audio::{AudioFileFormat, AudioFiles, AudioItem},
    mixer::AudioFilter,
};

use crate::SAMPLES_PER_SECOND;

const PRELOAD_NEXT_TRACK_BEFORE_END_DURATION_MS: u32 = 30000;
pub const DB_VOLTAGE_RATIO: f64 = 20.0;

// Spotify inserts a custom Ogg packet at the start with custom metadata values, that you would
// otherwise expect in Vorbis comments. This packet isn't well-formed and players may balk at it.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

pub type PlayerResult = Result<(), Error>;

pub struct Player {
    commands: Option<mpsc::UnboundedSender<PlayerCommand>>,
    thread_handle: Option<thread::JoinHandle<()>>,
    play_request_id_generator: SeqGenerator<u64>,
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum SinkStatus {
    Running,
    Closed,
    TemporarilyClosed,
}

pub type SinkEventCallback = Box<dyn Fn(SinkStatus) + Send>;

struct PlayerInternal {
    session: Session,
    config: PlayerConfig,
    commands: mpsc::UnboundedReceiver<PlayerCommand>,
    load_handles: Arc<Mutex<HashMap<thread::ThreadId, thread::JoinHandle<()>>>>,

    state: PlayerState,
    preload: PlayerPreload,
    sink: Box<dyn Sink>,
    sink_status: SinkStatus,
    sink_event_callback: Option<SinkEventCallback>,
    audio_filter: Option<Box<dyn AudioFilter + Send>>,
    event_senders: Vec<mpsc::UnboundedSender<PlayerEvent>>,
    converter: Converter,

    limiter_active: bool,
    limiter_attack_counter: u32,
    limiter_release_counter: u32,
    limiter_peak_sample: f64,
    limiter_factor: f64,
    limiter_strength: f64,

    auto_normalise_as_album: bool,
}

enum PlayerCommand {
    Load {
        track_id: SpotifyId,
        play_request_id: u64,
        play: bool,
        position_ms: u32,
    },
    Preload {
        track_id: SpotifyId,
    },
    Play,
    Pause,
    Stop,
    Seek(u32),
    AddEventSender(mpsc::UnboundedSender<PlayerEvent>),
    SetSinkEventCallback(Option<SinkEventCallback>),
    EmitVolumeSetEvent(u16),
    SetAutoNormaliseAsAlbum(bool),
    SkipExplicitContent(),
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    // Fired when the player is stopped (e.g. by issuing a "stop" command to the player).
    Stopped {
        play_request_id: u64,
        track_id: SpotifyId,
    },
    // The player started working on playback of a track while it was in a stopped state.
    // This is always immediately followed up by a "Loading" or "Playing" event.
    Started {
        play_request_id: u64,
        track_id: SpotifyId,
        position_ms: u32,
    },
    // Same as started but in the case that the player already had a track loaded.
    // The player was either playing the loaded track or it was paused.
    Changed {
        old_track_id: SpotifyId,
        new_track_id: SpotifyId,
    },
    // The player is delayed by loading a track.
    Loading {
        play_request_id: u64,
        track_id: SpotifyId,
        position_ms: u32,
    },
    // The player is preloading a track.
    Preloading {
        track_id: SpotifyId,
    },
    // The player is playing a track.
    // This event is issued at the start of playback of whenever the position must be communicated
    // because it is out of sync. This includes:
    // start of a track
    // un-pausing
    // after a seek
    // after a buffer-underrun
    Playing {
        play_request_id: u64,
        track_id: SpotifyId,
        position_ms: u32,
        duration_ms: u32,
    },
    // The player entered a paused state.
    Paused {
        play_request_id: u64,
        track_id: SpotifyId,
        position_ms: u32,
        duration_ms: u32,
    },
    // The player thinks it's a good idea to issue a preload command for the next track now.
    // This event is intended for use within spirc.
    TimeToPreloadNextTrack {
        play_request_id: u64,
        track_id: SpotifyId,
    },
    // The player reached the end of a track.
    // This event is intended for use within spirc. Spirc will respond by issuing another command
    // which will trigger another event (e.g. Changed or Stopped)
    EndOfTrack {
        play_request_id: u64,
        track_id: SpotifyId,
    },
    // The player was unable to load the requested track.
    Unavailable {
        play_request_id: u64,
        track_id: SpotifyId,
    },
    // The mixer volume was set to a new level.
    VolumeSet {
        volume: u16,
    },
}

impl PlayerEvent {
    pub fn get_play_request_id(&self) -> Option<u64> {
        use PlayerEvent::*;
        match self {
            Loading {
                play_request_id, ..
            }
            | Unavailable {
                play_request_id, ..
            }
            | Started {
                play_request_id, ..
            }
            | Playing {
                play_request_id, ..
            }
            | TimeToPreloadNextTrack {
                play_request_id, ..
            }
            | EndOfTrack {
                play_request_id, ..
            }
            | Paused {
                play_request_id, ..
            }
            | Stopped {
                play_request_id, ..
            } => Some(*play_request_id),
            Changed { .. } | Preloading { .. } | VolumeSet { .. } => None,
        }
    }
}

pub type PlayerEventChannel = mpsc::UnboundedReceiver<PlayerEvent>;

pub fn db_to_ratio(db: f64) -> f64 {
    f64::powf(10.0, db / DB_VOLTAGE_RATIO)
}

pub fn ratio_to_db(ratio: f64) -> f64 {
    ratio.log10() * DB_VOLTAGE_RATIO
}

#[derive(Clone, Copy, Debug)]
pub struct NormalisationData {
    // Spotify provides these as `f32`, but audio metadata can contain up to `f64`.
    // Also, this negates the need for casting during sample processing.
    pub track_gain_db: f64,
    pub track_peak: f64,
    pub album_gain_db: f64,
    pub album_peak: f64,
}

impl Default for NormalisationData {
    fn default() -> Self {
        Self {
            track_gain_db: 0.0,
            track_peak: 1.0,
            album_gain_db: 0.0,
            album_peak: 1.0,
        }
    }
}

impl NormalisationData {
    fn parse_from_ogg<T: Read + Seek>(mut file: T) -> io::Result<NormalisationData> {
        const SPOTIFY_NORMALIZATION_HEADER_START_OFFSET: u64 = 144;

        let newpos = file.seek(SeekFrom::Start(SPOTIFY_NORMALIZATION_HEADER_START_OFFSET))?;
        if newpos != SPOTIFY_NORMALIZATION_HEADER_START_OFFSET {
            error!(
                "NormalisationData::parse_from_file seeking to {} but position is now {}",
                SPOTIFY_NORMALIZATION_HEADER_START_OFFSET, newpos
            );
            error!("Falling back to default (non-track and non-album) normalisation data.");
            return Ok(NormalisationData::default());
        }

        let track_gain_db = file.read_f32::<LittleEndian>()? as f64;
        let track_peak = file.read_f32::<LittleEndian>()? as f64;
        let album_gain_db = file.read_f32::<LittleEndian>()? as f64;
        let album_peak = file.read_f32::<LittleEndian>()? as f64;

        let r = NormalisationData {
            track_gain_db,
            track_peak,
            album_gain_db,
            album_peak,
        };

        Ok(r)
    }

    fn get_factor(config: &PlayerConfig, data: NormalisationData) -> f64 {
        if !config.normalisation {
            return 1.0;
        }

        let [gain_db, gain_peak] = if config.normalisation_type == NormalisationType::Album {
            [data.album_gain_db, data.album_peak]
        } else {
            [data.track_gain_db, data.track_peak]
        };

        let normalisation_power = gain_db + config.normalisation_pregain;
        let mut normalisation_factor = db_to_ratio(normalisation_power);

        if normalisation_factor * gain_peak > config.normalisation_threshold {
            let limited_normalisation_factor = config.normalisation_threshold / gain_peak;
            let limited_normalisation_power = ratio_to_db(limited_normalisation_factor);

            if config.normalisation_method == NormalisationMethod::Basic {
                warn!("Limiting gain to {:.2} dB for the duration of this track to stay under normalisation threshold.", limited_normalisation_power);
                normalisation_factor = limited_normalisation_factor;
            } else {
                warn!(
                    "This track will at its peak be subject to {:.2} dB of dynamic limiting.",
                    normalisation_power - limited_normalisation_power
                );
            }

            warn!("Please lower pregain to avoid.");
        }

        debug!("Normalisation Data: {:?}", data);
        debug!(
            "Calculated Normalisation Factor for {:?}: {:.2}%",
            config.normalisation_type,
            normalisation_factor * 100.0
        );

        normalisation_factor
    }
}

impl Player {
    pub fn new<F>(
        config: PlayerConfig,
        session: Session,
        audio_filter: Option<Box<dyn AudioFilter + Send>>,
        sink_builder: F,
    ) -> (Player, PlayerEventChannel)
    where
        F: FnOnce() -> Box<dyn Sink> + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_sender, event_receiver) = mpsc::unbounded_channel();

        if config.normalisation {
            debug!("Normalisation Type: {:?}", config.normalisation_type);
            debug!(
                "Normalisation Pregain: {:.1} dB",
                config.normalisation_pregain
            );
            debug!(
                "Normalisation Threshold: {:.1} dBFS",
                ratio_to_db(config.normalisation_threshold)
            );
            debug!("Normalisation Method: {:?}", config.normalisation_method);

            if config.normalisation_method == NormalisationMethod::Dynamic {
                debug!("Normalisation Attack: {:?}", config.normalisation_attack);
                debug!("Normalisation Release: {:?}", config.normalisation_release);
                debug!("Normalisation Knee: {:?}", config.normalisation_knee);
            }
        }

        let handle = thread::spawn(move || {
            debug!("new Player[{}]", session.session_id());

            let converter = Converter::new(config.ditherer);

            let internal = PlayerInternal {
                session,
                config,
                commands: cmd_rx,
                load_handles: Arc::new(Mutex::new(HashMap::new())),

                state: PlayerState::Stopped,
                preload: PlayerPreload::None,
                sink: sink_builder(),
                sink_status: SinkStatus::Closed,
                sink_event_callback: None,
                audio_filter,
                event_senders: [event_sender].to_vec(),
                converter,

                limiter_active: false,
                limiter_attack_counter: 0,
                limiter_release_counter: 0,
                limiter_peak_sample: 0.0,
                limiter_factor: 1.0,
                limiter_strength: 0.0,

                auto_normalise_as_album: false,
            };

            // While PlayerInternal is written as a future, it still contains blocking code.
            // It must be run by using block_on() in a dedicated thread.
            let runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
            runtime.block_on(internal);

            debug!("PlayerInternal thread finished.");
        });

        (
            Player {
                commands: Some(cmd_tx),
                thread_handle: Some(handle),
                play_request_id_generator: SeqGenerator::new(0),
            },
            event_receiver,
        )
    }

    fn command(&self, cmd: PlayerCommand) {
        if let Some(commands) = self.commands.as_ref() {
            if let Err(e) = commands.send(cmd) {
                error!("Player Commands Error: {}", e);
            }
        }
    }

    pub fn load(&mut self, track_id: SpotifyId, start_playing: bool, position_ms: u32) -> u64 {
        let play_request_id = self.play_request_id_generator.get();
        self.command(PlayerCommand::Load {
            track_id,
            play_request_id,
            play: start_playing,
            position_ms,
        });

        play_request_id
    }

    pub fn preload(&self, track_id: SpotifyId) {
        self.command(PlayerCommand::Preload { track_id });
    }

    pub fn play(&self) {
        self.command(PlayerCommand::Play)
    }

    pub fn pause(&self) {
        self.command(PlayerCommand::Pause)
    }

    pub fn stop(&self) {
        self.command(PlayerCommand::Stop)
    }

    pub fn seek(&self, position_ms: u32) {
        self.command(PlayerCommand::Seek(position_ms));
    }

    pub fn get_player_event_channel(&self) -> PlayerEventChannel {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        self.command(PlayerCommand::AddEventSender(event_sender));
        event_receiver
    }

    pub async fn await_end_of_track(&self) {
        let mut channel = self.get_player_event_channel();
        while let Some(event) = channel.recv().await {
            if matches!(
                event,
                PlayerEvent::EndOfTrack { .. } | PlayerEvent::Stopped { .. }
            ) {
                return;
            }
        }
    }

    pub fn set_sink_event_callback(&self, callback: Option<SinkEventCallback>) {
        self.command(PlayerCommand::SetSinkEventCallback(callback));
    }

    pub fn emit_volume_set_event(&self, volume: u16) {
        self.command(PlayerCommand::EmitVolumeSetEvent(volume));
    }

    pub fn set_auto_normalise_as_album(&self, setting: bool) {
        self.command(PlayerCommand::SetAutoNormaliseAsAlbum(setting));
    }

    pub fn skip_explicit_content(&self) {
        self.command(PlayerCommand::SkipExplicitContent());
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        debug!("Shutting down player thread ...");
        self.commands = None;
        if let Some(handle) = self.thread_handle.take() {
            match handle.join() {
                Ok(_) => (),
                Err(e) => error!("Player thread Error: {:?}", e),
            }
        }
    }
}

struct PlayerLoadedTrackData {
    decoder: Decoder,
    normalisation_data: NormalisationData,
    stream_loader_controller: StreamLoaderController,
    bytes_per_second: usize,
    duration_ms: u32,
    stream_position_ms: u32,
    is_explicit: bool,
}

enum PlayerPreload {
    None,
    Loading {
        track_id: SpotifyId,
        loader: Pin<Box<dyn Future<Output = Result<PlayerLoadedTrackData, ()>> + Send>>,
    },
    Ready {
        track_id: SpotifyId,
        loaded_track: Box<PlayerLoadedTrackData>,
    },
}

type Decoder = Box<dyn AudioDecoder + Send>;

enum PlayerState {
    Stopped,
    Loading {
        track_id: SpotifyId,
        play_request_id: u64,
        start_playback: bool,
        loader: Pin<Box<dyn Future<Output = Result<PlayerLoadedTrackData, ()>> + Send>>,
    },
    Paused {
        track_id: SpotifyId,
        play_request_id: u64,
        decoder: Decoder,
        normalisation_data: NormalisationData,
        normalisation_factor: f64,
        stream_loader_controller: StreamLoaderController,
        bytes_per_second: usize,
        duration_ms: u32,
        stream_position_ms: u32,
        suggested_to_preload_next_track: bool,
        is_explicit: bool,
    },
    Playing {
        track_id: SpotifyId,
        play_request_id: u64,
        decoder: Decoder,
        normalisation_data: NormalisationData,
        normalisation_factor: f64,
        stream_loader_controller: StreamLoaderController,
        bytes_per_second: usize,
        duration_ms: u32,
        stream_position_ms: u32,
        reported_nominal_start_time: Option<Instant>,
        suggested_to_preload_next_track: bool,
        is_explicit: bool,
    },
    EndOfTrack {
        track_id: SpotifyId,
        play_request_id: u64,
        loaded_track: PlayerLoadedTrackData,
    },
    Invalid,
}

impl PlayerState {
    fn is_playing(&self) -> bool {
        use self::PlayerState::*;
        match *self {
            Stopped | EndOfTrack { .. } | Paused { .. } | Loading { .. } => false,
            Playing { .. } => true,
            Invalid => {
                error!("PlayerState::is_playing in invalid state");
                exit(1);
            }
        }
    }

    #[allow(dead_code)]
    fn is_stopped(&self) -> bool {
        use self::PlayerState::*;
        matches!(self, Stopped)
    }

    fn is_loading(&self) -> bool {
        use self::PlayerState::*;
        matches!(self, Loading { .. })
    }

    fn decoder(&mut self) -> Option<&mut Decoder> {
        use self::PlayerState::*;
        match *self {
            Stopped | EndOfTrack { .. } | Loading { .. } => None,
            Paused {
                ref mut decoder, ..
            }
            | Playing {
                ref mut decoder, ..
            } => Some(decoder),
            Invalid => {
                error!("PlayerState::decoder in invalid state");
                exit(1);
            }
        }
    }

    fn stream_loader_controller(&mut self) -> Option<&mut StreamLoaderController> {
        use self::PlayerState::*;
        match *self {
            Stopped | EndOfTrack { .. } | Loading { .. } => None,
            Paused {
                ref mut stream_loader_controller,
                ..
            }
            | Playing {
                ref mut stream_loader_controller,
                ..
            } => Some(stream_loader_controller),
            Invalid => {
                error!("PlayerState::stream_loader_controller in invalid state");
                exit(1);
            }
        }
    }

    fn playing_to_end_of_track(&mut self) {
        use self::PlayerState::*;
        let new_state = mem::replace(self, Invalid);
        match new_state {
            Playing {
                track_id,
                play_request_id,
                decoder,
                duration_ms,
                bytes_per_second,
                normalisation_data,
                stream_loader_controller,
                stream_position_ms,
                is_explicit,
                ..
            } => {
                *self = EndOfTrack {
                    track_id,
                    play_request_id,
                    loaded_track: PlayerLoadedTrackData {
                        decoder,
                        normalisation_data,
                        stream_loader_controller,
                        bytes_per_second,
                        duration_ms,
                        stream_position_ms,
                        is_explicit,
                    },
                };
            }
            _ => {
                error!(
                    "Called playing_to_end_of_track in non-playing state: {:?}",
                    new_state
                );
                exit(1);
            }
        }
    }

    fn paused_to_playing(&mut self) {
        use self::PlayerState::*;
        let new_state = mem::replace(self, Invalid);
        match new_state {
            Paused {
                track_id,
                play_request_id,
                decoder,
                normalisation_data,
                normalisation_factor,
                stream_loader_controller,
                duration_ms,
                bytes_per_second,
                stream_position_ms,
                suggested_to_preload_next_track,
                is_explicit,
            } => {
                *self = Playing {
                    track_id,
                    play_request_id,
                    decoder,
                    normalisation_data,
                    normalisation_factor,
                    stream_loader_controller,
                    duration_ms,
                    bytes_per_second,
                    stream_position_ms,
                    reported_nominal_start_time: None,
                    suggested_to_preload_next_track,
                    is_explicit,
                };
            }
            _ => {
                error!(
                    "PlayerState::paused_to_playing in invalid state: {:?}",
                    new_state
                );
                exit(1);
            }
        }
    }

    fn playing_to_paused(&mut self) {
        use self::PlayerState::*;
        let new_state = mem::replace(self, Invalid);
        match new_state {
            Playing {
                track_id,
                play_request_id,
                decoder,
                normalisation_data,
                normalisation_factor,
                stream_loader_controller,
                duration_ms,
                bytes_per_second,
                stream_position_ms,
                reported_nominal_start_time: _,
                suggested_to_preload_next_track,
                is_explicit,
            } => {
                *self = Paused {
                    track_id,
                    play_request_id,
                    decoder,
                    normalisation_data,
                    normalisation_factor,
                    stream_loader_controller,
                    duration_ms,
                    bytes_per_second,
                    stream_position_ms,
                    suggested_to_preload_next_track,
                    is_explicit,
                };
            }
            _ => {
                error!(
                    "PlayerState::playing_to_paused in invalid state: {:?}",
                    new_state
                );
                exit(1);
            }
        }
    }
}

struct PlayerTrackLoader {
    session: Session,
    config: PlayerConfig,
}

impl PlayerTrackLoader {
    async fn find_available_alternative(&self, audio: AudioItem) -> Option<AudioItem> {
        if let Err(e) = audio.availability {
            error!("Track is unavailable: {}", e);
            None
        } else if !audio.files.is_empty() {
            Some(audio)
        } else if let Some(alternatives) = &audio.alternatives {
            let alternatives: FuturesUnordered<_> = alternatives
                .iter()
                .map(|alt_id| AudioItem::get_file(&self.session, *alt_id))
                .collect();

            alternatives
                .filter_map(|x| future::ready(x.ok()))
                .filter(|x| future::ready(x.availability.is_ok()))
                .next()
                .await
        } else {
            error!("Track should be available, but no alternatives found.");
            None
        }
    }

    fn stream_data_rate(&self, format: AudioFileFormat) -> usize {
        let kbps = match format {
            AudioFileFormat::OGG_VORBIS_96 => 12,
            AudioFileFormat::OGG_VORBIS_160 => 20,
            AudioFileFormat::OGG_VORBIS_320 => 40,
            AudioFileFormat::MP3_256 => 32,
            AudioFileFormat::MP3_320 => 40,
            AudioFileFormat::MP3_160 => 20,
            AudioFileFormat::MP3_96 => 12,
            AudioFileFormat::MP3_160_ENC => 20,
            AudioFileFormat::AAC_24 => 3,
            AudioFileFormat::AAC_48 => 6,
            AudioFileFormat::FLAC_FLAC => 112, // assume 900 kbit/s on average
        };
        kbps * 1024
    }

    async fn load_track(
        &self,
        spotify_id: SpotifyId,
        position_ms: u32,
    ) -> Option<PlayerLoadedTrackData> {
        let audio = match AudioItem::get_file(&self.session, spotify_id).await {
            Ok(audio) => audio,
            Err(e) => {
                error!("Unable to load audio item: {:?}", e);
                return None;
            }
        };

        info!(
            "Loading <{}> with Spotify URI <{}>",
            audio.name, audio.spotify_uri
        );

        let is_explicit = audio.is_explicit;
        if is_explicit {
            if let Some(value) = self.session.get_user_attribute("filter-explicit-content") {
                if &value == "1" {
                    warn!("Track is marked as explicit, which client setting forbids.");
                    return None;
                }
            }
        }

        let audio = match self.find_available_alternative(audio).await {
            Some(audio) => audio,
            None => {
                error!("<{}> is not available", spotify_id.to_uri());
                return None;
            }
        };

        if audio.duration < 0 {
            error!(
                "Track duration for <{}> cannot be {}",
                spotify_id.to_uri(),
                audio.duration
            );
            return None;
        }
        let duration_ms = audio.duration as u32;

        // (Most) podcasts seem to support only 96 kbps Ogg Vorbis, so fall back to it
        let formats = match self.config.bitrate {
            Bitrate::Bitrate96 => [
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
            ],
            Bitrate::Bitrate160 => [
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
            ],
            Bitrate::Bitrate320 => [
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
            ],
        };

        let entry = formats.iter().find_map(|format| {
            if let Some(&file_id) = audio.files.get(format) {
                Some((*format, file_id))
            } else {
                None
            }
        });

        let (format, file_id) = match entry {
            Some(t) => t,
            None => {
                error!("<{}> is not available in any supported format", audio.name);
                return None;
            }
        };

        let bytes_per_second = self.stream_data_rate(format);
        let play_from_beginning = position_ms == 0;

        // This is only a loop to be able to reload the file if an error occured
        // while opening a cached file.
        loop {
            let encrypted_file = AudioFile::open(
                &self.session,
                file_id,
                bytes_per_second,
                play_from_beginning,
            );

            let encrypted_file = match encrypted_file.await {
                Ok(encrypted_file) => encrypted_file,
                Err(e) => {
                    error!("Unable to load encrypted file: {:?}", e);
                    return None;
                }
            };

            let is_cached = encrypted_file.is_cached();

            // Setting up demuxing and decoding will trigger a seek() so always start in random access mode.
            let stream_loader_controller = encrypted_file.get_stream_loader_controller().ok()?;
            stream_loader_controller.set_random_access_mode();

            // Not all audio files are encrypted. If we can't get a key, try loading the track
            // without decryption. If the file was encrypted after all, the decoder will fail
            // parsing and bail out, so we should be safe from outputting ear-piercing noise.
            let key = match self.session.audio_key().request(spotify_id, file_id).await {
                Ok(key) => Some(key),
                Err(e) => {
                    warn!("Unable to load key, continuing without decryption: {}", e);
                    None
                }
            };
            let mut decrypted_file = AudioDecrypt::new(key, encrypted_file);

            let is_ogg_vorbis = AudioFiles::is_ogg_vorbis(format);
            let (offset, mut normalisation_data) = if is_ogg_vorbis {
                // Spotify stores normalisation data in a custom Ogg packet instead of Vorbis comments.
                let normalisation_data =
                    NormalisationData::parse_from_ogg(&mut decrypted_file).ok();
                (SPOTIFY_OGG_HEADER_END, normalisation_data)
            } else {
                (0, None)
            };

            let audio_file = Subfile::new(
                decrypted_file,
                offset,
                stream_loader_controller.len() as u64,
            );

            let result = if self.config.passthrough {
                PassthroughDecoder::new(audio_file, format).map(|x| Box::new(x) as Decoder)
            } else {
                SymphoniaDecoder::new(audio_file, format).map(|mut decoder| {
                    // For formats other that Vorbis, we'll try getting normalisation data from
                    // ReplayGain metadata fields, if present.
                    if normalisation_data.is_none() {
                        normalisation_data = decoder.normalisation_data();
                    }
                    Box::new(decoder) as Decoder
                })
            };

            let normalisation_data = normalisation_data.unwrap_or_else(|| {
                warn!("Unable to get normalisation data, continuing with defaults.");
                NormalisationData::default()
            });

            let mut decoder = match result {
                Ok(decoder) => decoder,
                Err(e) if is_cached => {
                    warn!(
                        "Unable to read cached audio file: {}. Trying to download it.",
                        e
                    );

                    match self.session.cache() {
                        Some(cache) => {
                            if cache.remove_file(file_id).is_err() {
                                error!("Error removing file from cache");
                                return None;
                            }
                        }
                        None => {
                            error!("If the audio file is cached, a cache should exist");
                            return None;
                        }
                    }

                    // Just try it again
                    continue;
                }
                Err(e) => {
                    error!("Unable to read audio file: {}", e);
                    return None;
                }
            };

            // Ensure the starting position. Even when we want to play from the beginning,
            // the cursor may have been moved by parsing normalisation data. This may not
            // matter for playback (but won't hurt either), but may be useful for the
            // passthrough decoder.
            let stream_position_ms = match decoder.seek(position_ms) {
                Ok(_) => position_ms,
                Err(e) => {
                    warn!(
                        "PlayerTrackLoader::load_track error seeking to {}: {}",
                        position_ms, e
                    );
                    0
                }
            };

            // Transition from random access mode to streaming mode now that
            // we are ready to play from the requested position.
            stream_loader_controller.set_stream_mode();

            info!("<{}> ({} ms) loaded", audio.name, audio.duration);

            return Some(PlayerLoadedTrackData {
                decoder,
                normalisation_data,
                stream_loader_controller,
                bytes_per_second,
                duration_ms,
                stream_position_ms,
                is_explicit,
            });
        }
    }
}

impl Future for PlayerInternal {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // While this is written as a future, it still contains blocking code.
        // It must be run on its own thread.
        let passthrough = self.config.passthrough;

        loop {
            let mut all_futures_completed_or_not_ready = true;

            // process commands that were sent to us
            let cmd = match self.commands.poll_recv(cx) {
                Poll::Ready(None) => return Poll::Ready(()), // client has disconnected - shut down.
                Poll::Ready(Some(cmd)) => {
                    all_futures_completed_or_not_ready = false;
                    Some(cmd)
                }
                _ => None,
            };

            if let Some(cmd) = cmd {
                if let Err(e) = self.handle_command(cmd) {
                    error!("Error handling command: {}", e);
                }
            }

            // Handle loading of a new track to play
            if let PlayerState::Loading {
                ref mut loader,
                track_id,
                start_playback,
                play_request_id,
            } = self.state
            {
                match loader.as_mut().poll(cx) {
                    Poll::Ready(Ok(loaded_track)) => {
                        self.start_playback(
                            track_id,
                            play_request_id,
                            loaded_track,
                            start_playback,
                        );
                        if let PlayerState::Loading { .. } = self.state {
                            error!("The state wasn't changed by start_playback()");
                            exit(1);
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        error!(
                            "Skipping to next track, unable to load track <{:?}>: {:?}",
                            track_id, e
                        );
                        debug_assert!(self.state.is_loading());
                        self.send_event(PlayerEvent::Unavailable {
                            track_id,
                            play_request_id,
                        })
                    }
                    Poll::Pending => (),
                }
            }

            // handle pending preload requests.
            if let PlayerPreload::Loading {
                ref mut loader,
                track_id,
            } = self.preload
            {
                match loader.as_mut().poll(cx) {
                    Poll::Ready(Ok(loaded_track)) => {
                        self.send_event(PlayerEvent::Preloading { track_id });
                        self.preload = PlayerPreload::Ready {
                            track_id,
                            loaded_track: Box::new(loaded_track),
                        };
                    }
                    Poll::Ready(Err(_)) => {
                        debug!("Unable to preload {:?}", track_id);
                        self.preload = PlayerPreload::None;
                        // Let Spirc know that the track was unavailable.
                        if let PlayerState::Playing {
                            play_request_id, ..
                        }
                        | PlayerState::Paused {
                            play_request_id, ..
                        } = self.state
                        {
                            self.send_event(PlayerEvent::Unavailable {
                                track_id,
                                play_request_id,
                            });
                        }
                    }
                    Poll::Pending => (),
                }
            }

            if self.state.is_playing() {
                self.ensure_sink_running();

                if let PlayerState::Playing {
                    track_id,
                    play_request_id,
                    ref mut decoder,
                    normalisation_factor,
                    ref mut stream_position_ms,
                    ref mut reported_nominal_start_time,
                    duration_ms,
                    ..
                } = self.state
                {
                    match decoder.next_packet() {
                        Ok(result) => {
                            if let Some((new_stream_position_ms, ref packet)) = result {
                                if !passthrough {
                                    match packet.samples() {
                                        Ok(_) => {
                                            let notify_about_position =
                                                match *reported_nominal_start_time {
                                                    None => true,
                                                    Some(reported_nominal_start_time) => {
                                                        // only notify if we're behind. If we're ahead it's probably due to a buffer of the backend and we're actually in time.
                                                        let lag = (Instant::now()
                                                            - reported_nominal_start_time)
                                                            .as_millis()
                                                            as i64
                                                            - new_stream_position_ms as i64;
                                                        lag > Duration::from_secs(1).as_millis()
                                                            as i64
                                                    }
                                                };
                                            if notify_about_position {
                                                *reported_nominal_start_time = Some(
                                                    Instant::now()
                                                        - Duration::from_millis(
                                                            new_stream_position_ms as u64,
                                                        ),
                                                );
                                                self.send_event(PlayerEvent::Playing {
                                                    track_id,
                                                    play_request_id,
                                                    position_ms: new_stream_position_ms as u32,
                                                    duration_ms,
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            error!("Skipping to next track, unable to decode samples for track <{:?}>: {:?}", track_id, e);
                                            self.send_event(PlayerEvent::EndOfTrack {
                                                track_id,
                                                play_request_id,
                                            })
                                        }
                                    }
                                } else {
                                    // position, even if irrelevant, must be set so that seek() is called
                                    *stream_position_ms = new_stream_position_ms;
                                }
                            }

                            self.handle_packet(result, normalisation_factor);
                        }
                        Err(e) => {
                            error!("Skipping to next track, unable to get next packet for track <{:?}>: {:?}", track_id, e);
                            self.send_event(PlayerEvent::EndOfTrack {
                                track_id,
                                play_request_id,
                            })
                        }
                    }
                } else {
                    error!("PlayerInternal poll: Invalid PlayerState");
                    exit(1);
                };
            }

            if let PlayerState::Playing {
                track_id,
                play_request_id,
                duration_ms,
                stream_position_ms,
                ref mut stream_loader_controller,
                ref mut suggested_to_preload_next_track,
                ..
            }
            | PlayerState::Paused {
                track_id,
                play_request_id,
                duration_ms,
                stream_position_ms,
                ref mut stream_loader_controller,
                ref mut suggested_to_preload_next_track,
                ..
            } = self.state
            {
                if (!*suggested_to_preload_next_track)
                    && ((duration_ms as i64 - stream_position_ms as i64)
                        < PRELOAD_NEXT_TRACK_BEFORE_END_DURATION_MS as i64)
                    && stream_loader_controller.range_to_end_available()
                {
                    *suggested_to_preload_next_track = true;
                    self.send_event(PlayerEvent::TimeToPreloadNextTrack {
                        track_id,
                        play_request_id,
                    });
                }
            }

            if self.session.is_invalid() {
                return Poll::Ready(());
            }

            if (!self.state.is_playing()) && all_futures_completed_or_not_ready {
                return Poll::Pending;
            }
        }
    }
}

impl PlayerInternal {
    fn ensure_sink_running(&mut self) {
        if self.sink_status != SinkStatus::Running {
            trace!("== Starting sink ==");
            if let Some(callback) = &mut self.sink_event_callback {
                callback(SinkStatus::Running);
            }
            match self.sink.start() {
                Ok(()) => self.sink_status = SinkStatus::Running,
                Err(e) => {
                    error!("{}", e);
                    exit(1);
                }
            }
        }
    }

    fn ensure_sink_stopped(&mut self, temporarily: bool) {
        match self.sink_status {
            SinkStatus::Running => {
                trace!("== Stopping sink ==");
                match self.sink.stop() {
                    Ok(()) => {
                        self.sink_status = if temporarily {
                            SinkStatus::TemporarilyClosed
                        } else {
                            SinkStatus::Closed
                        };
                        if let Some(callback) = &mut self.sink_event_callback {
                            callback(self.sink_status);
                        }
                    }
                    Err(e) => {
                        error!("{}", e);
                        exit(1);
                    }
                }
            }
            SinkStatus::TemporarilyClosed => {
                if !temporarily {
                    self.sink_status = SinkStatus::Closed;
                    if let Some(callback) = &mut self.sink_event_callback {
                        callback(SinkStatus::Closed);
                    }
                }
            }
            SinkStatus::Closed => (),
        }
    }

    fn handle_player_stop(&mut self) {
        match self.state {
            PlayerState::Playing {
                track_id,
                play_request_id,
                ..
            }
            | PlayerState::Paused {
                track_id,
                play_request_id,
                ..
            }
            | PlayerState::EndOfTrack {
                track_id,
                play_request_id,
                ..
            }
            | PlayerState::Loading {
                track_id,
                play_request_id,
                ..
            } => {
                self.ensure_sink_stopped(false);
                self.send_event(PlayerEvent::Stopped {
                    track_id,
                    play_request_id,
                });
                self.state = PlayerState::Stopped;
            }
            PlayerState::Stopped => (),
            PlayerState::Invalid => {
                error!("PlayerInternal::handle_player_stop in invalid state");
                exit(1);
            }
        }
    }

    fn handle_play(&mut self) {
        if let PlayerState::Paused {
            track_id,
            play_request_id,
            stream_position_ms,
            duration_ms,
            ..
        } = self.state
        {
            self.state.paused_to_playing();
            self.send_event(PlayerEvent::Playing {
                track_id,
                play_request_id,
                position_ms: stream_position_ms,
                duration_ms,
            });
            self.ensure_sink_running();
        } else {
            error!("Player::play called from invalid state: {:?}", self.state);
        }
    }

    fn handle_pause(&mut self) {
        if let PlayerState::Playing {
            track_id,
            play_request_id,
            stream_position_ms,
            duration_ms,
            ..
        } = self.state
        {
            self.state.playing_to_paused();

            self.ensure_sink_stopped(false);
            self.send_event(PlayerEvent::Paused {
                track_id,
                play_request_id,
                position_ms: stream_position_ms,
                duration_ms,
            });
        } else {
            error!("Player::pause called from invalid state: {:?}", self.state);
        }
    }

    fn handle_packet(&mut self, packet: Option<(u32, AudioPacket)>, normalisation_factor: f64) {
        match packet {
            Some((_, mut packet)) => {
                if !packet.is_empty() {
                    if let AudioPacket::Samples(ref mut data) = packet {
                        if self.config.normalisation
                            && !(f64::abs(normalisation_factor - 1.0) <= f64::EPSILON
                                && self.config.normalisation_method == NormalisationMethod::Basic)
                        {
                            for sample in data.iter_mut() {
                                let mut actual_normalisation_factor = normalisation_factor;
                                if self.config.normalisation_method == NormalisationMethod::Dynamic
                                {
                                    if self.limiter_active {
                                        // "S"-shaped curve with a configurable knee during attack and release:
                                        //  - > 1.0 yields soft knees at start and end, steeper in between
                                        //  - 1.0 yields a linear function from 0-100%
                                        //  - between 0.0 and 1.0 yields hard knees at start and end, flatter in between
                                        //  - 0.0 yields a step response to 50%, causing distortion
                                        //  - Rates < 0.0 invert the limiter and are invalid
                                        let mut shaped_limiter_strength = self.limiter_strength;
                                        if shaped_limiter_strength > 0.0
                                            && shaped_limiter_strength < 1.0
                                        {
                                            shaped_limiter_strength = 1.0
                                                / (1.0
                                                    + f64::powf(
                                                        shaped_limiter_strength
                                                            / (1.0 - shaped_limiter_strength),
                                                        -self.config.normalisation_knee,
                                                    ));
                                        }
                                        actual_normalisation_factor =
                                            (1.0 - shaped_limiter_strength) * normalisation_factor
                                                + shaped_limiter_strength * self.limiter_factor;
                                    };

                                    // Cast the fields here for better readability
                                    let normalisation_attack =
                                        self.config.normalisation_attack.as_secs_f64();
                                    let normalisation_release =
                                        self.config.normalisation_release.as_secs_f64();
                                    let limiter_release_counter =
                                        self.limiter_release_counter as f64;
                                    let limiter_attack_counter = self.limiter_attack_counter as f64;
                                    let samples_per_second = SAMPLES_PER_SECOND as f64;

                                    // Always check for peaks, even when the limiter is already active.
                                    // There may be even higher peaks than we initially targeted.
                                    // Check against the normalisation factor that would be applied normally.
                                    let abs_sample = f64::abs(*sample * normalisation_factor);
                                    if abs_sample > self.config.normalisation_threshold {
                                        self.limiter_active = true;
                                        if self.limiter_release_counter > 0 {
                                            // A peak was encountered while releasing the limiter;
                                            // synchronize with the current release limiter strength.
                                            self.limiter_attack_counter = (((samples_per_second
                                                * normalisation_release)
                                                - limiter_release_counter)
                                                / (normalisation_release / normalisation_attack))
                                                as u32;
                                            self.limiter_release_counter = 0;
                                        }

                                        self.limiter_attack_counter =
                                            self.limiter_attack_counter.saturating_add(1);

                                        self.limiter_strength = limiter_attack_counter
                                            / (samples_per_second * normalisation_attack);

                                        if abs_sample > self.limiter_peak_sample {
                                            self.limiter_peak_sample = abs_sample;
                                            self.limiter_factor =
                                                self.config.normalisation_threshold
                                                    / self.limiter_peak_sample;
                                        }
                                    } else if self.limiter_active {
                                        if self.limiter_attack_counter > 0 {
                                            // Release may start within the attack period, before
                                            // the limiter reached full strength. For that reason
                                            // start the release by synchronizing with the current
                                            // attack limiter strength.
                                            self.limiter_release_counter = (((samples_per_second
                                                * normalisation_attack)
                                                - limiter_attack_counter)
                                                * (normalisation_release / normalisation_attack))
                                                as u32;
                                            self.limiter_attack_counter = 0;
                                        }

                                        self.limiter_release_counter =
                                            self.limiter_release_counter.saturating_add(1);

                                        if self.limiter_release_counter
                                            > (samples_per_second * normalisation_release) as u32
                                        {
                                            self.reset_limiter();
                                        } else {
                                            self.limiter_strength = ((samples_per_second
                                                * normalisation_release)
                                                - limiter_release_counter)
                                                / (samples_per_second * normalisation_release);
                                        }
                                    }
                                }
                                *sample *= actual_normalisation_factor;
                            }
                        }

                        if let Some(ref editor) = self.audio_filter {
                            editor.modify_stream(data)
                        }
                    }

                    if let Err(e) = self.sink.write(&packet, &mut self.converter) {
                        error!("{}", e);
                        exit(1);
                    }
                }
            }

            None => {
                self.state.playing_to_end_of_track();
                if let PlayerState::EndOfTrack {
                    track_id,
                    play_request_id,
                    ..
                } = self.state
                {
                    self.send_event(PlayerEvent::EndOfTrack {
                        track_id,
                        play_request_id,
                    })
                } else {
                    error!("PlayerInternal handle_packet: Invalid PlayerState");
                    exit(1);
                }
            }
        }
    }

    fn reset_limiter(&mut self) {
        self.limiter_active = false;
        self.limiter_release_counter = 0;
        self.limiter_attack_counter = 0;
        self.limiter_peak_sample = 0.0;
        self.limiter_factor = 1.0;
        self.limiter_strength = 0.0;
    }

    fn start_playback(
        &mut self,
        track_id: SpotifyId,
        play_request_id: u64,
        loaded_track: PlayerLoadedTrackData,
        start_playback: bool,
    ) {
        let position_ms = loaded_track.stream_position_ms;

        let mut config = self.config.clone();
        if config.normalisation_type == NormalisationType::Auto {
            if self.auto_normalise_as_album {
                config.normalisation_type = NormalisationType::Album;
            } else {
                config.normalisation_type = NormalisationType::Track;
            }
        };
        let normalisation_factor =
            NormalisationData::get_factor(&config, loaded_track.normalisation_data);

        if start_playback {
            self.ensure_sink_running();

            self.send_event(PlayerEvent::Playing {
                track_id,
                play_request_id,
                position_ms,
                duration_ms: loaded_track.duration_ms,
            });

            self.state = PlayerState::Playing {
                track_id,
                play_request_id,
                decoder: loaded_track.decoder,
                normalisation_data: loaded_track.normalisation_data,
                normalisation_factor,
                stream_loader_controller: loaded_track.stream_loader_controller,
                duration_ms: loaded_track.duration_ms,
                bytes_per_second: loaded_track.bytes_per_second,
                stream_position_ms: loaded_track.stream_position_ms,
                reported_nominal_start_time: Some(
                    Instant::now() - Duration::from_millis(position_ms as u64),
                ),
                suggested_to_preload_next_track: false,
                is_explicit: loaded_track.is_explicit,
            };
        } else {
            self.ensure_sink_stopped(false);

            self.state = PlayerState::Paused {
                track_id,
                play_request_id,
                decoder: loaded_track.decoder,
                normalisation_data: loaded_track.normalisation_data,
                normalisation_factor,
                stream_loader_controller: loaded_track.stream_loader_controller,
                duration_ms: loaded_track.duration_ms,
                bytes_per_second: loaded_track.bytes_per_second,
                stream_position_ms: loaded_track.stream_position_ms,
                suggested_to_preload_next_track: false,
                is_explicit: loaded_track.is_explicit,
            };

            self.send_event(PlayerEvent::Paused {
                track_id,
                play_request_id,
                position_ms,
                duration_ms: loaded_track.duration_ms,
            });
        }
    }

    fn handle_command_load(
        &mut self,
        track_id: SpotifyId,
        play_request_id: u64,
        play: bool,
        position_ms: u32,
    ) {
        if !self.config.gapless {
            self.ensure_sink_stopped(play);
        }
        // emit the correct player event
        match self.state {
            PlayerState::Playing {
                track_id: old_track_id,
                ..
            }
            | PlayerState::Paused {
                track_id: old_track_id,
                ..
            }
            | PlayerState::EndOfTrack {
                track_id: old_track_id,
                ..
            }
            | PlayerState::Loading {
                track_id: old_track_id,
                ..
            } => self.send_event(PlayerEvent::Changed {
                old_track_id,
                new_track_id: track_id,
            }),
            PlayerState::Stopped => self.send_event(PlayerEvent::Started {
                track_id,
                play_request_id,
                position_ms,
            }),
            PlayerState::Invalid { .. } => {
                error!(
                    "Player::handle_command_load called from invalid state: {:?}",
                    self.state
                );
                exit(1);
            }
        }

        // Now we check at different positions whether we already have a pre-loaded version
        // of this track somewhere. If so, use it and return.

        // Check if there's a matching loaded track in the EndOfTrack player state.
        // This is the case if we're repeating the same track again.
        if let PlayerState::EndOfTrack {
            track_id: previous_track_id,
            ..
        } = self.state
        {
            if previous_track_id == track_id {
                let mut loaded_track = match mem::replace(&mut self.state, PlayerState::Invalid) {
                    PlayerState::EndOfTrack { loaded_track, .. } => loaded_track,
                    _ => {
                        error!("PlayerInternal handle_command_load: Invalid PlayerState");
                        exit(1);
                    }
                };

                if position_ms != loaded_track.stream_position_ms {
                    loaded_track
                        .stream_loader_controller
                        .set_random_access_mode();
                    // This may be blocking.
                    match loaded_track.decoder.seek(position_ms) {
                        Ok(_) => loaded_track.stream_position_ms = position_ms,
                        Err(e) => error!("PlayerInternal handle_command_load: {}", e),
                    }
                    loaded_track.stream_loader_controller.set_stream_mode();
                }
                self.preload = PlayerPreload::None;
                self.start_playback(track_id, play_request_id, loaded_track, play);
                if let PlayerState::Invalid = self.state {
                    error!("start_playback() hasn't set a valid player state.");
                    exit(1);
                }
                return;
            }
        }

        // Check if we are already playing the track. If so, just do a seek and update our info.
        if let PlayerState::Playing {
            track_id: current_track_id,
            ref mut stream_position_ms,
            ref mut decoder,
            ref mut stream_loader_controller,
            ..
        }
        | PlayerState::Paused {
            track_id: current_track_id,
            ref mut stream_position_ms,
            ref mut decoder,
            ref mut stream_loader_controller,
            ..
        } = self.state
        {
            if current_track_id == track_id {
                // we can use the current decoder. Ensure it's at the correct position.
                if position_ms != *stream_position_ms {
                    stream_loader_controller.set_random_access_mode();
                    // This may be blocking.
                    match decoder.seek(position_ms) {
                        Ok(_) => *stream_position_ms = position_ms,
                        Err(e) => {
                            error!("PlayerInternal::handle_command_load error seeking: {}", e)
                        }
                    }
                    stream_loader_controller.set_stream_mode();
                }

                // Move the info from the current state into a PlayerLoadedTrackData so we can use
                // the usual code path to start playback.
                let old_state = mem::replace(&mut self.state, PlayerState::Invalid);

                if let PlayerState::Playing {
                    stream_position_ms,
                    decoder,
                    stream_loader_controller,
                    bytes_per_second,
                    duration_ms,
                    normalisation_data,
                    is_explicit,
                    ..
                }
                | PlayerState::Paused {
                    stream_position_ms,
                    decoder,
                    stream_loader_controller,
                    bytes_per_second,
                    duration_ms,
                    normalisation_data,
                    is_explicit,
                    ..
                } = old_state
                {
                    let loaded_track = PlayerLoadedTrackData {
                        decoder,
                        normalisation_data,
                        stream_loader_controller,
                        bytes_per_second,
                        duration_ms,
                        stream_position_ms,
                        is_explicit,
                    };

                    self.preload = PlayerPreload::None;
                    self.start_playback(track_id, play_request_id, loaded_track, play);

                    if let PlayerState::Invalid = self.state {
                        error!("start_playback() hasn't set a valid player state.");
                        exit(1);
                    }

                    return;
                } else {
                    error!("PlayerInternal handle_command_load: Invalid PlayerState");
                    exit(1);
                }
            }
        }

        // Check if the requested track has been preloaded already. If so use the preloaded data.
        if let PlayerPreload::Ready {
            track_id: loaded_track_id,
            ..
        } = self.preload
        {
            if track_id == loaded_track_id {
                let preload = std::mem::replace(&mut self.preload, PlayerPreload::None);
                if let PlayerPreload::Ready {
                    track_id,
                    mut loaded_track,
                } = preload
                {
                    if position_ms != loaded_track.stream_position_ms {
                        loaded_track
                            .stream_loader_controller
                            .set_random_access_mode();
                        // This may be blocking
                        match loaded_track.decoder.seek(position_ms) {
                            Ok(_) => loaded_track.stream_position_ms = position_ms,
                            Err(e) => error!("PlayerInternal handle_command_load: {}", e),
                        }
                        loaded_track.stream_loader_controller.set_stream_mode();
                    }
                    self.start_playback(track_id, play_request_id, *loaded_track, play);
                    return;
                } else {
                    error!("PlayerInternal handle_command_load: Invalid PlayerState");
                    exit(1);
                }
            }
        }

        // We need to load the track - either from scratch or by completing a preload.
        // In any case we go into a Loading state to load the track.
        self.ensure_sink_stopped(play);

        self.send_event(PlayerEvent::Loading {
            track_id,
            play_request_id,
            position_ms,
        });

        // Try to extract a pending loader from the preloading mechanism
        let loader = if let PlayerPreload::Loading {
            track_id: loaded_track_id,
            ..
        } = self.preload
        {
            if (track_id == loaded_track_id) && (position_ms == 0) {
                let mut preload = PlayerPreload::None;
                std::mem::swap(&mut preload, &mut self.preload);
                if let PlayerPreload::Loading { loader, .. } = preload {
                    Some(loader)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        self.preload = PlayerPreload::None;

        // If we don't have a loader yet, create one from scratch.
        let loader = loader.unwrap_or_else(|| Box::pin(self.load_track(track_id, position_ms)));

        // Set ourselves to a loading state.
        self.state = PlayerState::Loading {
            track_id,
            play_request_id,
            start_playback: play,
            loader,
        };
    }

    fn handle_command_preload(&mut self, track_id: SpotifyId) {
        debug!("Preloading track");
        let mut preload_track = true;
        // check whether the track is already loaded somewhere or being loaded.
        if let PlayerPreload::Loading {
            track_id: currently_loading,
            ..
        }
        | PlayerPreload::Ready {
            track_id: currently_loading,
            ..
        } = self.preload
        {
            if currently_loading == track_id {
                // we're already preloading the requested track.
                preload_track = false;
            } else {
                // we're preloading something else - cancel it.
                self.preload = PlayerPreload::None;
            }
        }

        if let PlayerState::Playing {
            track_id: current_track_id,
            ..
        }
        | PlayerState::Paused {
            track_id: current_track_id,
            ..
        }
        | PlayerState::EndOfTrack {
            track_id: current_track_id,
            ..
        } = self.state
        {
            if current_track_id == track_id {
                // we already have the requested track loaded.
                preload_track = false;
            }
        }

        // schedule the preload of the current track if desired.
        if preload_track {
            let loader = self.load_track(track_id, 0);
            self.preload = PlayerPreload::Loading {
                track_id,
                loader: Box::pin(loader),
            }
        }
    }

    fn handle_command_seek(&mut self, position_ms: u32) -> PlayerResult {
        if let Some(stream_loader_controller) = self.state.stream_loader_controller() {
            stream_loader_controller.set_random_access_mode();
        }
        if let Some(decoder) = self.state.decoder() {
            match decoder.seek(position_ms) {
                Ok(_) => {
                    if let PlayerState::Playing {
                        ref mut stream_position_ms,
                        ..
                    }
                    | PlayerState::Paused {
                        ref mut stream_position_ms,
                        ..
                    } = self.state
                    {
                        *stream_position_ms = position_ms;
                    }
                }
                Err(e) => error!("PlayerInternal::handle_command_seek error: {}", e),
            }
        } else {
            error!("Player::seek called from invalid state: {:?}", self.state);
        }

        // If we're playing, ensure, that we have enough data leaded to avoid a buffer underrun.
        if let Some(stream_loader_controller) = self.state.stream_loader_controller() {
            stream_loader_controller.set_stream_mode();
        }

        // ensure we have a bit of a buffer of downloaded data
        self.preload_data_before_playback()?;

        if let PlayerState::Playing {
            track_id,
            play_request_id,
            ref mut reported_nominal_start_time,
            duration_ms,
            ..
        } = self.state
        {
            *reported_nominal_start_time =
                Some(Instant::now() - Duration::from_millis(position_ms as u64));
            self.send_event(PlayerEvent::Playing {
                track_id,
                play_request_id,
                position_ms,
                duration_ms,
            });
        }
        if let PlayerState::Paused {
            track_id,
            play_request_id,
            duration_ms,
            ..
        } = self.state
        {
            self.send_event(PlayerEvent::Paused {
                track_id,
                play_request_id,
                position_ms,
                duration_ms,
            });
        }

        Ok(())
    }

    fn handle_command(&mut self, cmd: PlayerCommand) -> PlayerResult {
        debug!("command={:?}", cmd);
        let result = match cmd {
            PlayerCommand::Load {
                track_id,
                play_request_id,
                play,
                position_ms,
            } => self.handle_command_load(track_id, play_request_id, play, position_ms),

            PlayerCommand::Preload { track_id } => self.handle_command_preload(track_id),

            PlayerCommand::Seek(position_ms) => self.handle_command_seek(position_ms)?,

            PlayerCommand::Play => self.handle_play(),

            PlayerCommand::Pause => self.handle_pause(),

            PlayerCommand::Stop => self.handle_player_stop(),

            PlayerCommand::AddEventSender(sender) => self.event_senders.push(sender),

            PlayerCommand::SetSinkEventCallback(callback) => self.sink_event_callback = callback,

            PlayerCommand::EmitVolumeSetEvent(volume) => {
                self.send_event(PlayerEvent::VolumeSet { volume })
            }

            PlayerCommand::SetAutoNormaliseAsAlbum(setting) => {
                self.auto_normalise_as_album = setting
            }

            PlayerCommand::SkipExplicitContent() => {
                if let PlayerState::Playing {
                    track_id,
                    play_request_id,
                    is_explicit,
                    ..
                }
                | PlayerState::Paused {
                    track_id,
                    play_request_id,
                    is_explicit,
                    ..
                } = self.state
                {
                    if is_explicit {
                        warn!("Currently loaded track is explicit, which client setting forbids -- skipping to next track.");
                        self.send_event(PlayerEvent::EndOfTrack {
                            track_id,
                            play_request_id,
                        })
                    }
                }
            }
        };

        Ok(result)
    }

    fn send_event(&mut self, event: PlayerEvent) {
        let mut index = 0;
        while index < self.event_senders.len() {
            match self.event_senders[index].send(event.clone()) {
                Ok(_) => index += 1,
                Err(_) => {
                    self.event_senders.remove(index);
                }
            }
        }
    }

    fn load_track(
        &mut self,
        spotify_id: SpotifyId,
        position_ms: u32,
    ) -> impl Future<Output = Result<PlayerLoadedTrackData, ()>> + Send + 'static {
        // This method creates a future that returns the loaded stream and associated info.
        // Ideally all work should be done using asynchronous code. However, seek() on the
        // audio stream is implemented in a blocking fashion. Thus, we can't turn it into future
        // easily. Instead we spawn a thread to do the work and return a one-shot channel as the
        // future to work with.

        let loader = PlayerTrackLoader {
            session: self.session.clone(),
            config: self.config.clone(),
        };

        let (result_tx, result_rx) = oneshot::channel();

        let load_handles_clone = self.load_handles.clone();
        let handle = tokio::runtime::Handle::current();
        let load_handle = thread::spawn(move || {
            let data = handle.block_on(loader.load_track(spotify_id, position_ms));
            if let Some(data) = data {
                let _ = result_tx.send(data);
            }

            let mut load_handles = load_handles_clone.lock();
            load_handles.remove(&thread::current().id());
        });

        let mut load_handles = self.load_handles.lock();
        load_handles.insert(load_handle.thread().id(), load_handle);

        result_rx.map_err(|_| ())
    }

    fn preload_data_before_playback(&mut self) -> PlayerResult {
        if let PlayerState::Playing {
            bytes_per_second,
            ref mut stream_loader_controller,
            ..
        } = self.state
        {
            // Request our read ahead range
            let request_data_length = max(
                (READ_AHEAD_DURING_PLAYBACK_ROUNDTRIPS
                    * stream_loader_controller.ping_time().as_secs_f32()
                    * bytes_per_second as f32) as usize,
                (READ_AHEAD_DURING_PLAYBACK.as_secs_f32() * bytes_per_second as f32) as usize,
            );
            stream_loader_controller.fetch_next(request_data_length);

            // Request the part we want to wait for blocking. This effecively means we wait for the previous request to partially complete.
            let wait_for_data_length = max(
                (READ_AHEAD_BEFORE_PLAYBACK_ROUNDTRIPS
                    * stream_loader_controller.ping_time().as_secs_f32()
                    * bytes_per_second as f32) as usize,
                (READ_AHEAD_BEFORE_PLAYBACK.as_secs_f32() * bytes_per_second as f32) as usize,
            );
            stream_loader_controller
                .fetch_next_blocking(wait_for_data_length)
                .map_err(Into::into)
        } else {
            Ok(())
        }
    }
}

impl Drop for PlayerInternal {
    fn drop(&mut self) {
        debug!("drop PlayerInternal[{}]", self.session.session_id());

        let handles: Vec<thread::JoinHandle<()>> = {
            // waiting for the thread while holding the mutex would result in a deadlock
            let mut load_handles = self.load_handles.lock();

            load_handles
                .drain()
                .map(|(_thread_id, handle)| handle)
                .collect()
        };

        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl fmt::Debug for PlayerCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PlayerCommand::Load {
                track_id,
                play,
                position_ms,
                ..
            } => f
                .debug_tuple("Load")
                .field(&track_id)
                .field(&play)
                .field(&position_ms)
                .finish(),
            PlayerCommand::Preload { track_id } => {
                f.debug_tuple("Preload").field(&track_id).finish()
            }
            PlayerCommand::Play => f.debug_tuple("Play").finish(),
            PlayerCommand::Pause => f.debug_tuple("Pause").finish(),
            PlayerCommand::Stop => f.debug_tuple("Stop").finish(),
            PlayerCommand::Seek(position) => f.debug_tuple("Seek").field(&position).finish(),
            PlayerCommand::AddEventSender(_) => f.debug_tuple("AddEventSender").finish(),
            PlayerCommand::SetSinkEventCallback(_) => {
                f.debug_tuple("SetSinkEventCallback").finish()
            }
            PlayerCommand::EmitVolumeSetEvent(volume) => {
                f.debug_tuple("VolumeSet").field(&volume).finish()
            }
            PlayerCommand::SetAutoNormaliseAsAlbum(setting) => f
                .debug_tuple("SetAutoNormaliseAsAlbum")
                .field(&setting)
                .finish(),
            PlayerCommand::SkipExplicitContent() => f.debug_tuple("SkipExplicitContent").finish(),
        }
    }
}

impl fmt::Debug for PlayerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use PlayerState::*;
        match *self {
            Stopped => f.debug_struct("Stopped").finish(),
            Loading {
                track_id,
                play_request_id,
                ..
            } => f
                .debug_struct("Loading")
                .field("track_id", &track_id)
                .field("play_request_id", &play_request_id)
                .finish(),
            Paused {
                track_id,
                play_request_id,
                ..
            } => f
                .debug_struct("Paused")
                .field("track_id", &track_id)
                .field("play_request_id", &play_request_id)
                .finish(),
            Playing {
                track_id,
                play_request_id,
                ..
            } => f
                .debug_struct("Playing")
                .field("track_id", &track_id)
                .field("play_request_id", &play_request_id)
                .finish(),
            EndOfTrack {
                track_id,
                play_request_id,
                ..
            } => f
                .debug_struct("EndOfTrack")
                .field("track_id", &track_id)
                .field("play_request_id", &play_request_id)
                .finish(),
            Invalid => f.debug_struct("Invalid").finish(),
        }
    }
}

struct Subfile<T: Read + Seek> {
    stream: T,
    offset: u64,
    length: u64,
}

impl<T: Read + Seek> Subfile<T> {
    pub fn new(mut stream: T, offset: u64, length: u64) -> Subfile<T> {
        let target = SeekFrom::Start(offset);
        match stream.seek(target) {
            Ok(pos) => {
                if pos != offset {
                    error!(
                        "Subfile::new seeking to {:?} but position is now {:?}",
                        target, pos
                    );
                }
            }
            Err(e) => error!("Subfile new Error: {}", e),
        }

        Subfile {
            stream,
            offset,
            length,
        }
    }
}

impl<T: Read + Seek> Read for Subfile<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Read + Seek> Seek for Subfile<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(offset) => SeekFrom::Start(offset + self.offset),
            x => x,
        };

        let newpos = self.stream.seek(pos)?;

        if newpos >= self.offset {
            Ok(newpos - self.offset)
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "newpos < self.offset",
            ))
        }
    }
}

impl<R> MediaSource for Subfile<R>
where
    R: Read + Seek + Send,
{
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}
