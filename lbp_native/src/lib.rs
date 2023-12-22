use std::{
    ffi::CStr,
    fmt::Debug,
    io::BufWriter,
    num::NonZeroU64,
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant, SystemTime},
};

use flume::{Receiver, RecvTimeoutError, Sender};
use num_enum::FromPrimitive;
use parking_lot::Mutex;

use jni::{
    objects::{GlobalRef, JClass, JObject, JString, JValueGen},
    sys::{jbyte, jint},
    JNIEnv,
};
use regex::Regex;
use serde::{Serialize, Serializer};
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
pub struct TrackMetadata {
    additional_info: AdditionalInfo,
    artist_name: String,
    track_name: String,
    release_name: String,
}

fn serialize_artist_mbids<S>(mbids: &Vec<String>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let uuid_regex = UUID_REGEX.get().unwrap();
    s.collect_seq(
        mbids
            .iter()
            .flat_map(|mbid| uuid_regex.find(mbid).map(|m| m.as_str())),
    )
}

#[derive(Serialize, Debug)]
struct AdditionalInfo {
    media_player: &'static str,
    submission_client: &'static str,
    submission_client_version: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    release_mbid: String,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_artist_mbids"
    )]
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
            media_player: "PowerAmp",
            submission_client: "ListenBrainz PowerAmp",
            submission_client_version: env!("CARGO_PKG_VERSION"),
            release_mbid: String::new(),
            artist_mbids: Vec::new(),
            recording_mbid: String::new(),
            duration_ms: 0,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    TrackChanged(TrackMetadata, jint, Instant, bool),
    StateChanged(PowerampState),
    SetToken(String),
}

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct MetadataReqFlags: jbyte {
        const ARTIST = 1;
        const TITLE = 2;
        const ALBUM = 4;
        const RELEASE_MBID = 8;
        const ARTIST_MBIDS = 16;
        const RECORDING_MBID = 32;
    }
}

impl std::fmt::Display for MetadataReqFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        bitflags::parser::to_writer(self, f)
    }
}

#[derive(Debug, Default, FromPrimitive)]
#[repr(i32)]
pub enum PowerampState {
    #[default]
    NoState = -1,
    Stopped = 0,
    Playing = 1,
    Paused = 2,
}

fn scrobble(listen_type: &'static str, payload: &Payload, token: &str, cache_path: &Path) {
    let send = ListenbrainzSingleListen {
        listen_type,
        payload: [payload],
    };
    #[cfg(debug_assertions)]
    log::debug!("{}", serde_json::to_string_pretty(&send).unwrap());
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
        for i in std::fs::read_dir(&cache_path).unwrap().map(|f| f.unwrap()) {
            let path = i.path();
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
        log::debug!("{}", unsafe { std::str::from_utf8_unchecked(&request) });
        let status = ureq::post("https://api.listenbrainz.org/1/submit-listens")
            .set("Authorization", token)
            .set("Content-Type", "json")
            .send_bytes(&request);
        if status.is_err() {
            log::debug!("Error importing {:?}", status);
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

static EVENT_LOOP_SENDER: Mutex<Option<Sender<Event>>> = Mutex::new(None);
static UUID_REGEX: OnceLock<Regex> = OnceLock::new();
static JOBJECT: OnceLock<GlobalRef> = OnceLock::new();

fn init_thread(event: Event, env: &mut JNIEnv, lock: &mut Option<Sender<Event>>) {
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
    let token = token_c_str.to_str().unwrap().to_string();
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
    let cache_path = Path::new(cache_path_c_str.to_str().unwrap()).join("listenbrainz");
    if !cache_path.exists() {
        std::fs::create_dir(&cache_path).unwrap();
    }
    let mut data = ListenbrainzData {
        token,
        cache_path,
        ..Default::default()
    };
    import_cache(&data.token, &data.cache_path);
    // Maximum 2 events at a time. Track, and Status
    let (tx, rx): (Sender<Event>, Receiver<Event>) = flume::bounded(2);

    *lock = Some(tx);
    std::thread::spawn(move || {
        log::info!("Opening thread");

        handle_event(event, &mut data);
        'mainloop: loop {
            let event = if data.timeout {
                if Instant::now() >= data.scrobble_deadline {
                    log::info!("Waiting");
                    rx.recv().map_err(|e| e.into())
                } else {
                    log::info!(
                        "Waiting: {:?}",
                        data.scrobble_deadline.duration_since(Instant::now())
                    );
                    rx.recv_deadline(data.scrobble_deadline)
                }
            } else {
                log::info!("Waiting");
                rx.recv().map_err(|e| e.into())
            };
            match event {
                Ok(event) => handle_event(event, &mut data),
                Err(RecvTimeoutError::Timeout) => {
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
                Err(RecvTimeoutError::Disconnected) => break 'mainloop,
            }
        }
        log::info!("Closing thread");
    });
}

fn handle_event(event: Event, data: &mut ListenbrainzData) {
    match event {
        Event::TrackChanged(metadata, pos, now, data_scrobble) => {
            data.payload.track_metadata = metadata;
            let pos = Duration::from_secs(pos as _);

            data.scrobble = data_scrobble;
            data.timeout = data.scrobble && !data.paused;

            if data.scrobble {
                let mut scrobble_deadline = Duration::from_millis(scrobble_duration!(
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
                scrobble("playing_now", &data.payload, &data.token, &data.cache_path);
            }
        }
        Event::StateChanged(state) => match state {
            PowerampState::Paused => {
                data.pause_instant = Instant::now();
                data.timeout = false;
                data.paused = true;
            }
            PowerampState::Playing => {
                data.scrobble_deadline = data.scrobble_deadline + data.pause_instant.elapsed();
                data.timeout = true;
                data.paused = false;
            }
            // Receiver will get disconnected anyway
            PowerampState::NoState | PowerampState::Stopped => {}
        },
        Event::SetToken(token) => {
            data.token = token;
        }
    }
}

fn send_event(event: Event, env: &mut JNIEnv) {
    let mut lock = EVENT_LOOP_SENDER.lock();
    if let Some(tx) = std::ops::Deref::deref(&lock) {
        tx.send(event).unwrap();
    } else {
        init_thread(event, env, &mut lock);
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
    /*
    std::panic::set_hook(Box::new(|panic_info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");

        let msg = match panic_info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match panic_info.payload().downcast_ref::<String>() {
                Some(s) => &**s,
                None => "Box<Any>",
            },
        };

        let mut path = String::new();
        for path_num in 0.. {
            path = format!("/storage/emulated/0/listenbrainz-{}.crash", path_num);
            match Path::new(&path).try_exists() {
                Ok(true) => break,
                Ok(false) => continue,
                Err(_) => return,
            }
        }

        match panic_info.location() {
            Some(location) => {
                let _ = std::fs::write(
                    &path,
                    format!(
                        "panic on thread '{}' panicked at '{}': {}:{}",
                        thread,
                        msg,
                        location.file(),
                        location.line(),
                    ),
                );
            }
            None => {
                let _ = std::fs::write(
                    &path,
                    format!("panic on thread '{}' panicked at '{}'", thread, msg),
                );
            }
        }
    }));
    */
    log_panics::init();
    JOBJECT.set(env.new_global_ref(callback).unwrap()).unwrap();
    UUID_REGEX
        .set(Regex::new("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap())
        .unwrap();
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
    metadata_reqs: jbyte,
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
            let metadata_reqs = MetadataReqFlags::from_bits(metadata_reqs).unwrap();
            log::debug!("Reqs: {}", metadata_reqs);
            let mut scrobble = true;
            for req in metadata_reqs {
                match req {
                    MetadataReqFlags::ARTIST => {
                        scrobble = scrobble && !track_metadata.artist_name.is_empty()
                    }
                    MetadataReqFlags::TITLE => {
                        scrobble = scrobble && !track_metadata.track_name.is_empty()
                    }
                    MetadataReqFlags::ALBUM => {
                        scrobble = scrobble && !track_metadata.release_name.is_empty()
                    }
                    MetadataReqFlags::RELEASE_MBID => {
                        scrobble =
                            scrobble && !track_metadata.additional_info.release_mbid.is_empty()
                    }
                    MetadataReqFlags::ARTIST_MBIDS => {
                        scrobble =
                            scrobble && !track_metadata.additional_info.artist_mbids.is_empty()
                    }
                    MetadataReqFlags::RECORDING_MBID => {
                        scrobble =
                            scrobble && !track_metadata.additional_info.recording_mbid.is_empty()
                    }
                    _ => unreachable!(),
                }
            }
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
            let mut lock = EVENT_LOOP_SENDER.lock();
            *lock = None;
            env.call_method(JOBJECT.get().unwrap(), "threadStopped", "()V", &[])
                .unwrap();
        }
        _ => {
            send_event(Event::StateChanged(state), &mut env);
        }
    }
}
