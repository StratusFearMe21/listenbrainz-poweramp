use std::{
    ffi::CStr,
    io::BufWriter,
    num::NonZeroU64,
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime},
};

use num_enum::FromPrimitive;
use parking_lot::Mutex;

use jni::{
    objects::{GlobalRef, JClass, JObject, JString, JValueGen},
    sys::jint,
    JNIEnv,
};
use polling::Poller;
use serde::Serialize;
use symphonia::core::{
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::{MetadataOptions, StandardTagKey, Value},
    probe::Hint,
};

#[derive(Debug)]
struct ListenbrainzData {
    payload: Payload,
    scrobble: bool,
    token: String,
    cache_path: PathBuf,
    scrobble_deadline: Instant,
    timeout: bool,
    paused: bool,
    pause_instant: Instant,
}

impl Default for ListenbrainzData {
    fn default() -> Self {
        Self {
            payload: Payload::default(),
            scrobble: false,
            token: String::new(),
            cache_path: PathBuf::new(),
            scrobble_deadline: Instant::now(),
            timeout: false,
            paused: true,
            pause_instant: Instant::now(),
        }
    }
}

#[derive(Serialize, Debug)]
struct ListenbrainzSingleListen<'a> {
    listen_type: &'static str,
    payload: [&'a Payload; 1],
}

#[derive(Serialize, Default, Debug)]
struct Payload {
    #[serde(skip_serializing_if = "Option::is_none")]
    listened_at: Option<NonZeroU64>,
    track_metadata: TrackMetadata,
}

#[derive(Serialize, Default, Debug)]
struct TrackMetadata {
    additional_info: AdditionalInfo,
    artist_name: String,
    track_name: String,
    release_name: String,
}

#[derive(Serialize, Debug)]
struct AdditionalInfo {
    media_player: &'static str,
    submission_client: &'static str,
    submission_client_version: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    release_mbid: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    artist_mbids: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    recording_mbid: String,
    duration_ms: u64,
}

#[derive(Serialize, Default, Debug)]
struct LoveHate<'a> {
    recording_mbid: &'a str,
    score: i32,
}

impl Default for AdditionalInfo {
    fn default() -> Self {
        Self {
            media_player: "mpv",
            submission_client: "ListenBrainz PowerAmp",
            submission_client_version: env!("CARGO_PKG_VERSION"),
            release_mbid: String::new(),
            artist_mbids: Vec::new(),
            recording_mbid: String::new(),
            duration_ms: 0,
        }
    }
}

enum Event {
    TrackChanged(TrackMetadata, jint, Instant, bool),
    StateChanged(PowerampState),
    SetToken(String),
}

#[derive(Debug, Default, FromPrimitive)]
#[repr(i32)]
enum PowerampState {
    #[default]
    NoState = -1,
    Stopped = 0,
    Playing = 1,
    Paused = 2,
}

#[derive(Debug)]
pub struct NotifierSender<T> {
    sender: Sender<T>,
    poller: Arc<Poller>,
}

impl<T> NotifierSender<T> {
    /// Send a message to the channel
    ///
    /// This will wake the event loop and deliver an `Event::Msg` to
    /// it containing the provided value.
    pub fn send(&self, t: T) -> Result<(), std::io::Error> {
        self.sender
            .send(t)
            .map(|()| self.poller.notify())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::NotConnected))?
    }
}

fn scrobble(listen_type: &'static str, payload: &Payload, token: &str, cache_path: &Path) {
    let send = ListenbrainzSingleListen {
        listen_type,
        payload: [payload],
    };
    #[cfg(debug_assertions)]
    eprintln!("{}", serde_json::to_string_pretty(&send).unwrap());
    let status = ureq::post("https://api.listenbrainz.org/1/submit-listens")
        .set("Authorization", token)
        .send_json(send);
    if status.is_ok() {
        import_cache(token, cache_path);
        return;
    }
    if let Some(listened_at) = payload.listened_at {
        serde_json::to_writer(
            BufWriter::new(
                std::fs::File::create(cache_path.join(format!("{}.json", listened_at))).unwrap(),
            ),
            &payload,
        )
        .unwrap();
    }
}

fn import_cache(token: &str, cache_path: &Path) {
    let mut read_dir = cache_path.read_dir().unwrap();
    let is_occupied = read_dir.next().is_some();
    let is_one_file = read_dir.next().is_none();
    if cache_path.exists() && is_occupied {
        let mut request = if is_one_file {
            br#"{"listen_type":"single","payload":["#.to_vec()
        } else {
            br#"{"listen_type":"import","payload":["#.to_vec()
        };
        for i in std::fs::read_dir(&cache_path).unwrap() {
            let path = i.unwrap().path();
            std::io::copy(
                &mut std::fs::File::open(path.as_path()).unwrap(),
                &mut request,
            )
            .unwrap();
            request.push(b',');
        }
        request.pop();
        request.extend_from_slice(b"]}");
        #[cfg(debug_assertions)]
        eprintln!("{}", unsafe { std::str::from_utf8_unchecked(&request) });
        let status = ureq::post("https://api.listenbrainz.org/1/submit-listens")
            .set("Authorization", token)
            .set("Content-Type", "json")
            .send_bytes(&request);
        if status.is_err() {
            eprintln!("Error importing {:?}", status);
            return;
        }
        std::fs::read_dir(cache_path)
            .unwrap()
            .try_for_each(|i| std::fs::remove_file(i?.path()))
            .unwrap();
    }
}

macro_rules! scrobble_duration {
    ($duration:expr,$speed:expr) => {
        if $duration <= 40_000 {
            $duration - 1_000
        } else {
            u64::min(240_000, $duration / 2)
        } / $speed
    };
}

static EVENT_LOOP_SENDER: Mutex<Option<NotifierSender<Event>>> = Mutex::new(None);
static JOBJECT: OnceLock<GlobalRef> = OnceLock::new();

fn init_thread(rx: Receiver<Event>, token: String, cache_path: PathBuf, poller: Arc<Poller>) {
    std::thread::spawn(move || {
        let mut data = ListenbrainzData {
            token,
            cache_path,
            ..Default::default()
        };
        import_cache(&data.token, &data.cache_path);
        let mut events = Vec::new();

        'mainloop: loop {
            events.clear();
            poller
                .wait(
                    &mut events,
                    if data.timeout {
                        match data.scrobble_deadline.duration_since(Instant::now()) {
                            Duration::ZERO => None,
                            timeout => Some(timeout),
                        }
                    } else {
                        None
                    },
                )
                .unwrap();
            let mut iter = rx.try_iter().peekable();
            match iter.peek() {
                Some(_) => {
                    for event in iter {
                        match event {
                            Event::TrackChanged(metadata, pos, now, data_scrobble) => {
                                data.payload.track_metadata = metadata;
                                let pos = Duration::from_secs(pos as _);

                                data.scrobble = data_scrobble;

                                if data.scrobble {
                                    let mut scrobble_deadline =
                                        Duration::from_millis(scrobble_duration!(
                                            data.payload.track_metadata.additional_info.duration_ms,
                                            1
                                        ));

                                    if pos < scrobble_deadline {
                                        scrobble_deadline -= pos;
                                    } else {
                                        data.scrobble = false;
                                        return;
                                    }

                                    data.scrobble_deadline = now + scrobble_deadline;

                                    data.payload.listened_at = None;
                                    scrobble(
                                        "playing_now",
                                        &data.payload,
                                        &data.token,
                                        &data.cache_path,
                                    );
                                }
                                data.timeout = data.scrobble && !data.paused;
                            }
                            Event::StateChanged(state) => match state {
                                PowerampState::NoState | PowerampState::Stopped => {
                                    *EVENT_LOOP_SENDER.lock() = None;
                                    break 'mainloop;
                                }
                                PowerampState::Paused => {
                                    data.pause_instant = Instant::now();
                                    data.timeout = false;
                                    data.paused = true;
                                }
                                PowerampState::Playing => {
                                    data.scrobble_deadline =
                                        data.scrobble_deadline + data.pause_instant.elapsed();
                                    data.timeout = true;
                                    data.paused = false;
                                }
                            },
                            Event::SetToken(token) => data.token = token,
                        }
                    }
                }
                None => {
                    if data.scrobble {
                        data.payload.listened_at = NonZeroU64::new(
                            SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs(),
                        );
                        scrobble("single", &data.payload, &data.token, &data.cache_path);
                    }
                    data.scrobble = false;
                    data.timeout = false;
                }
            }
        }
    });
}

fn send_event(event: Event, env: &mut JNIEnv) {
    let mut lock = EVENT_LOOP_SENDER.lock();
    if let Some(tx) = std::ops::Deref::deref(&lock) {
        tx.send(event).unwrap();
    } else {
        let (tx, rx): (Sender<Event>, Receiver<Event>) = std::sync::mpsc::channel();

        let token = env
            .call_method(
                JOBJECT.get().unwrap(),
                "getToken",
                "()Ljava/lang/String;",
                &[],
            )
            .unwrap();
        let token_jstring = match token {
            JValueGen::Object(o) => JString::from(o),
            _ => unreachable!(),
        };
        let token_javastr = env.get_string(&token_jstring).unwrap();
        let token_c_str = unsafe { CStr::from_ptr(token_javastr.as_ptr()) };
        let token_rust = token_c_str.to_str().unwrap().to_string();
        let cache_path = env
            .call_method(
                JOBJECT.get().unwrap(),
                "getCache",
                "()Ljava/lang/String;",
                &[],
            )
            .unwrap();
        let cache_path_jstring = match cache_path {
            JValueGen::Object(o) => JString::from(o),
            _ => unreachable!(),
        };
        let cache_path_javastr = env.get_string(&cache_path_jstring).unwrap();
        let cache_path_c_str = unsafe { CStr::from_ptr(cache_path_javastr.as_ptr()) };
        let cache_path_rust = Path::new(cache_path_c_str.to_str().unwrap()).to_path_buf();
        let poller = Arc::new(Poller::new().unwrap());
        init_thread(rx, token_rust, cache_path_rust, Arc::clone(&poller));
        tx.send(event).unwrap();
        *lock = Some(NotifierSender { sender: tx, poller });
    }
}

#[no_mangle]
pub extern "system" fn Java_com_example_listenbrainzpoweramp_ForegroundService_initrs(
    env: JNIEnv,
    _: JClass,
    callback: JObject,
) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Trace),
    );
    log_panics::init();
    JOBJECT.set(env.new_global_ref(callback).unwrap()).unwrap();
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_example_listenbrainzpoweramp_ForegroundService_setToken(
    mut env: JNIEnv,
    _: JClass,
    token: JString,
) {
    let lock = EVENT_LOOP_SENDER.lock();
    if let Some(tx) = std::ops::Deref::deref(&lock) {
        let token_java_str = env.get_string(&token).unwrap();
        let token_c_str = CStr::from_ptr(token_java_str.as_ptr());
        let token_rust = token_c_str.to_str().unwrap().to_string();
        tx.send(Event::SetToken(token_rust)).unwrap();
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_example_listenbrainzpoweramp_ForegroundService_mTrackFunction(
    mut env: JNIEnv,
    _: JClass,
    path: JString,
    ext: JString,
    dur: jint,
    pos: jint,
) {
    let now = Instant::now();

    let mut track_metadata = TrackMetadata::default();
    track_metadata.additional_info.duration_ms = dur as u64;

    let path_java_str = env.get_string(&path).unwrap();
    let path_c_str = CStr::from_ptr(path_java_str.as_ptr());
    let path_rust = path_c_str.to_str().unwrap();
    log::debug!("Path: {}", path_rust);

    let file = if path_rust.starts_with("fd://") {
        Ok(std::fs::File::from_raw_fd(path_rust[5..].parse().unwrap()))
    } else {
        std::fs::File::open(path_rust)
    };

    // Open the media source.
    match file {
        Ok(src) => {
            // Create the media source stream.
            let mss = MediaSourceStream::new(Box::new(src), Default::default());

            // Create a probe hint using the file's extension. [Optional]
            let mut hint = Hint::new();
            let ext_java_str = env.get_string(&ext).unwrap();
            let ext_c_str = CStr::from_ptr(ext_java_str.as_ptr());
            let ext_rust = ext_c_str.to_str().unwrap();
            log::debug!("Extension: {}", ext_rust);
            hint.with_extension(ext_rust);

            // Use the default options for metadata and format readers.
            let meta_opts: MetadataOptions = Default::default();
            let fmt_opts: FormatOptions = Default::default();

            // Probe the media source.
            let probed = symphonia::default::get_probe()
                .format(&hint, mss, &fmt_opts, &meta_opts)
                .expect("unsupported format");

            let mut probed_metadata_vec = Vec::new();
            let mut metadata_vec = Vec::new();

            let mut metadata = probed.metadata;
            let mut format = probed.format;

            if let Some(mut m) = metadata.get() {
                if let Some(latest) = m.skip_to_latest() {
                    std::mem::swap(&mut latest.tags, &mut probed_metadata_vec);
                }
            }

            let mut metadata = format.metadata();

            if let Some(latest) = metadata.skip_to_latest() {
                std::mem::swap(&mut latest.tags, &mut metadata_vec);
            }

            for tag in probed_metadata_vec.drain(..).chain(metadata_vec.drain(..)) {
                match tag.std_key {
                    Some(StandardTagKey::Artist) => {
                        track_metadata.artist_name = {
                            let Value::String(tag) = tag.value else {
                        unreachable!()
                    };

                            tag
                        }
                    }
                    Some(StandardTagKey::TrackTitle) => {
                        track_metadata.track_name = {
                            let Value::String(tag) = tag.value else {
                        unreachable!()
                    };

                            tag
                        }
                    }
                    Some(StandardTagKey::Album) => {
                        track_metadata.release_name = {
                            let Value::String(tag) = tag.value else {
                        unreachable!()
                    };

                            tag
                        }
                    }
                    Some(StandardTagKey::MusicBrainzAlbumId) => {
                        track_metadata.additional_info.release_mbid = {
                            let Value::String(tag) = tag.value else {
                        unreachable!()
                    };

                            tag
                        }
                    }
                    Some(StandardTagKey::MusicBrainzArtistId) => {
                        track_metadata.additional_info.artist_mbids.push({
                            let Value::String(tag) = tag.value else {
                        unreachable!()
                    };

                            tag
                        })
                    }
                    Some(StandardTagKey::MusicBrainzRecordingId) => {
                        track_metadata.additional_info.recording_mbid = match tag.value {
                            Value::String(tag) => tag,
                            Value::Binary(tag) => String::from_utf8(Vec::from(tag)).unwrap(),
                            _ => unreachable!(),
                        };
                    }
                    _ => {}
                }
            }

            log::debug!("{:#?}", track_metadata);
            let scrobble = !track_metadata.artist_name.is_empty()
                && !track_metadata.track_name.is_empty()
                && !track_metadata.release_name.is_empty()
                && !track_metadata.additional_info.release_mbid.is_empty();
            if scrobble {
                env.call_method(JOBJECT.get().unwrap(), "isScrobbling", "()V", &[])
                    .unwrap();
            } else {
                env.call_method(JOBJECT.get().unwrap(), "notScrobbling", "()V", &[])
                    .unwrap();
            }
            send_event(
                Event::TrackChanged(track_metadata, pos, now, scrobble),
                &mut env,
            );
        }
        Err(e) => {
            log::error!("{:#?}", e);
            env.call_method(JOBJECT.get().unwrap(), "notScrobbling", "()V", &[])
                .unwrap();
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_example_listenbrainzpoweramp_ForegroundService_mStatusFunction(
    mut env: JNIEnv,
    _: JClass,
    state: jint,
) {
    let state = PowerampState::from(state);
    log::debug!("State: {:?}", state);
    match state {
        PowerampState::NoState | PowerampState::Stopped => {
            let lock_occupied = {
                let lock = EVENT_LOOP_SENDER.lock();
                lock.is_some()
            };
            if lock_occupied {
                env.call_method(JOBJECT.get().unwrap(), "threadStopped", "()V", &[])
                    .unwrap();
            }
        }
        _ => {}
    }
    send_event(Event::StateChanged(state), &mut env);
}
