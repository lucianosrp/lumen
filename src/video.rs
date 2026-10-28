//! In-viewer video playback on GStreamer: playbin3 streams from Immich, an appsink hands
//! RGBA frames to the UI thread, which uploads the latest one into a texture.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

/// Videos shorter than this loop, like Live Photos; longer ones stop at the end.
const LOOP_UNDER: f64 = 15.0;
/// Settings → Loop short clips.
pub static LOOP_SHORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Initialises GStreamer once; false if it's unusable (the viewer falls back to mpv).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match gst::init() {
        Ok(()) => {
            if std::env::var_os("LUMEN_NO_HWDEC").is_some_and(|v| !v.is_empty()) {
                demote_hw_decoders();
            }
            true
        }
        Err(e) => {
            log::warn!("GStreamer unavailable, videos will open in mpv: {e}");
            false
        }
    })
}

/// Whether to convert and scale on the GPU (CUDA) rather than on the CPU.
static CUDA: AtomicBool = AtomicBool::new(true);

fn cuda() -> bool {
    CUDA.load(Ordering::Relaxed)
        && ["cudaupload", "cudaconvertscale", "cudadownload"].iter().all(|f| gst::ElementFactory::find(f).is_some())
}

/// Stops autoplugging NVDEC decoders, so decodebin picks the software (avdec) ones, and
/// keeps conversion on the CPU.
fn demote_hw_decoders() {
    CUDA.store(false, Ordering::Relaxed);
    for f in gst::ElementFactory::factories_with_type(gst::ElementFactoryType::DECODER, gst::Rank::NONE) {
        if f.name().starts_with("nv") {
            f.set_rank(gst::Rank::NONE);
        }
    }
}

/// The newest decoded frame, tightly packed RGBA. Reused between frames to avoid allocating.
#[derive(Default)]
struct Slot {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    pts: f64,
    fresh: bool,
}

#[derive(Clone)]
pub struct Options {
    /// Frames are scaled down in the pipeline so neither side exceeds this.
    pub max_side: u32,
    /// Sends audio to a fakesink (headless tests).
    pub silent: bool,
    pub start_at: Option<f64>,
    pub muted: bool,
    pub volume: f64,
    /// Loop at the end: always, never, or (None) only clips shorter than `LOOP_UNDER`.
    pub looping: Option<bool>,
    /// Don't even decode the audio track (grid hover previews).
    pub no_audio: bool,
}

impl Options {
    /// A muted, looping preview whose frames fit `max_side` (grid hover, Live Photo motion).
    pub fn preview(max_side: u32) -> Self {
        Self { max_side, silent: true, start_at: None, muted: true, volume: 0.0, looping: Some(true), no_audio: true }
    }
}

/// Pipelines built and not yet torn down, for leak checks in the logs.
static ALIVE: AtomicUsize = AtomicUsize::new(0);

pub struct Player {
    playbin: gst::Element,
    slot: Arc<Mutex<Slot>>,
    /// What the user asked for; buffering and seeking pause underneath it.
    want_playing: bool,
    state: gst::State,
    prerolled: bool,
    pending_seek: Option<(f64, bool)>,
    /// Where the seek in flight is going; reported as the position until it lands.
    seek_target: Option<f64>,
    buffering: Option<i32>,
    duration: Option<f64>,
    pub ended: bool,
    pub failed: bool,
    /// The hardware decoder broke; the caller should rebuild the player (now on avdec).
    pub retry: bool,
    pub frames: u64,
    pub last_pts: f64,
    logged: bool,
    looping: Option<bool>,
}

impl Player {
    pub fn new(uri: &str, api_key: &str, opts: Options, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let playbin = gst::ElementFactory::make("playbin3").property("uri", uri).build().context("playbin3")?;
        playbin.set_property("mute", opts.muted);
        playbin.set_property("volume", cubic(opts.volume));

        // The key goes into a header on the HTTP source only, never into the URI or logs.
        let key = api_key.to_string();
        playbin.connect("source-setup", false, move |args| {
            if let Some(src) = args.get(1).and_then(|v| v.get::<gst::Element>().ok())
                && src.find_property("extra-headers").is_some()
            {
                let headers = gst::Structure::builder("headers").field("x-api-key", key.as_str()).build();
                src.set_property("extra-headers", &headers);
            }
            None
        });

        // Scale and convert on the way to the appsink so the UI copies at most max_side².
        // With CUDA that happens on the GPU (NVDEC frames never touch the CPU until they're
        // small RGBA; software-decoded ones get uploaded), which costs ~4x less CPU.
        // videoflip applies phone rotation tags; it sits after the scaler where frames are small.
        let gpu = cuda();
        let convert = if gpu {
            "cudaupload ! cudaconvertscale add-borders=false ! capsfilter name=size ! cudadownload"
        } else {
            "videoconvertscale add-borders=false n-threads=2 ! capsfilter name=size"
        };
        let sink_bin = gst::parse::bin_from_description(
            &format!("{convert} ! videoflip video-direction=auto ! video/x-raw,format=RGBA ! appsink name=sink"),
            true,
        )?;
        let capsfilter = sink_bin.by_name("size").context("capsfilter")?;
        let rgba = move |size: Option<(i32, i32)>| {
            let mut caps = gst::Caps::builder("video/x-raw").field("format", "RGBA");
            if let Some((w, h)) = size {
                caps = caps.field("width", w).field("height", h).field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
            }
            let mut caps = caps.build();
            if gpu {
                caps.make_mut().set_features(0, Some(gst::CapsFeatures::new(["memory:CUDAMemory"])));
            }
            caps
        };
        capsfilter.set_property("caps", rgba(None));
        let max_side = opts.max_side.max(64);
        // The scaler can't keep the aspect ratio inside a bounding box on its own, so pick the
        // output size from the decoder's caps before they reach it.
        let ghost = sink_bin.static_pad("sink").context("sink pad")?;
        ghost.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view())
                && let Ok(vi) = gst_video::VideoInfo::from_caps(c.caps())
            {
                let par = vi.par();
                let dw = vi.width() as f64 * par.numer() as f64 / par.denom().max(1) as f64;
                let dh = vi.height() as f64;
                let k = (max_side as f64 / dw.max(dh)).min(1.0);
                let even = |v: f64| ((v * k / 2.0).round() as i32 * 2).max(2);
                capsfilter.set_property("caps", rgba(Some((even(dw), even(dh)))));
            }
            gst::PadProbeReturn::Ok
        });

        let appsink = sink_bin.by_name("sink").context("appsink")?.downcast::<gst_app::AppSink>().ok().context("appsink")?;
        appsink.set_max_buffers(2);
        appsink.set_drop(true);
        appsink.set_sync(true);
        appsink.set_caps(Some(&gst::Caps::builder("video/x-raw").field("format", "RGBA").build()));
        let slot = Arc::new(Mutex::new(Slot::default()));
        let store = {
            let (slot, wake) = (slot.clone(), wake.clone());
            move |sample: gst::Sample| {
                if copy_frame(&sample, &slot).is_some() {
                    wake();
                }
            }
        };
        let store2 = store.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    store(s.pull_sample().map_err(|_| gst::FlowError::Eos)?);
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |s| {
                    store2(s.pull_preroll().map_err(|_| gst::FlowError::Eos)?);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        playbin.set_property("video-sink", &sink_bin);
        if opts.silent {
            let fake = gst::ElementFactory::make("fakesink").property("sync", true).build()?;
            playbin.set_property("audio-sink", &fake);
        }

        // Wake the UI for bus messages; it drains them in `tick`.
        let bus = playbin.bus().context("bus")?;
        let weak = playbin.downgrade();
        let video_only = opts.no_audio;
        bus.set_sync_handler(move |_, msg| {
            // Video only: pick the streams right here, before decodebin3 exposes (and the
            // sinks wait to preroll on) an audio stream we'd drop later.
            if let gst::MessageView::StreamCollection(c) = msg.view()
                && video_only
                && let Some(playbin) = weak.upgrade()
            {
                let ids: Vec<String> = c
                    .stream_collection()
                    .iter()
                    .filter(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                    .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                    .take(1)
                    .collect();
                if !ids.is_empty() {
                    playbin.send_event(gst::event::SelectStreams::new(ids.iter().map(String::as_str)));
                }
            }
            wake();
            gst::BusSyncReply::Pass
        });

        // Preroll paused first so a start position can be applied before anything plays.
        playbin.set_state(gst::State::Paused).context("couldn't start the pipeline")?;
        let alive = ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
        log::info!("video: pipeline up ({max_side}px max, {alive} alive)");
        Ok(Self {
            playbin,
            slot,
            want_playing: true,
            state: gst::State::Null,
            prerolled: false,
            pending_seek: opts.start_at.filter(|t| *t > 0.0).map(|t| (t, true)),
            seek_target: opts.start_at,
            buffering: None,
            duration: None,
            ended: false,
            failed: false,
            retry: false,
            frames: 0,
            last_pts: 0.0,
            logged: false,
            looping: opts.looping,
        })
    }

    /// Drains the bus: state, buffering, end of stream and errors.
    pub fn tick(&mut self) {
        let Some(bus) = self.playbin.bus() else { return };
        while let Some(msg) = bus.pop() {
            use gst::MessageView as M;
            let from_playbin = msg.src().is_some_and(|s| s == &self.playbin);
            match msg.view() {
                M::StateChanged(s) if from_playbin => {
                    self.state = s.current();
                    if self.state == gst::State::Playing && !self.logged {
                        self.logged = true;
                        self.log_elements();
                    }
                }
                M::AsyncDone(_) => {
                    self.prerolled = true;
                    self.seek_target = None;
                    if let Some((t, accurate)) = self.pending_seek.take() {
                        self.seek_now(t, accurate);
                    } else {
                        self.apply_state();
                    }
                }
                M::Buffering(b) => {
                    let p = b.percent();
                    self.buffering = (p < 100).then_some(p);
                    self.apply_state();
                }
                M::DurationChanged(_) => self.duration = None,
                M::Eos(_) => {
                    let short = self.duration.is_some_and(|d| d < LOOP_UNDER);
                    let loop_short = LOOP_SHORT.load(std::sync::atomic::Ordering::Relaxed);
                    if self.looping.unwrap_or(short && loop_short) && self.want_playing {
                        self.seek_now(0.0, false);
                    } else {
                        self.ended = true;
                        self.want_playing = false;
                        self.apply_state();
                    }
                }
                M::Error(e) => {
                    let src = msg.src().map(|s| s.path_string().to_string()).unwrap_or_default();
                    log::warn!("video: {src}: {} ({})", e.error(), e.debug().unwrap_or_default());
                    let hw = msg
                        .src()
                        .and_then(|s| s.downcast_ref::<gst::Element>())
                        .and_then(|e| e.factory())
                        .is_some_and(|f| f.name().starts_with("nv") || f.name().starts_with("cuda"));
                    if hw && CUDA.load(Ordering::Relaxed) {
                        log::warn!("video: hardware decoding failed, retrying with software decoders");
                        demote_hw_decoders();
                        self.retry = true;
                    } else {
                        self.failed = true;
                    }
                }
                _ => {}
            }
        }
        if self.duration.is_none() && self.prerolled {
            self.duration = self.playbin.query_duration::<gst::ClockTime>().map(|t| t.seconds_f64()).filter(|d| *d > 0.0);
        }
    }

    /// Logs what GStreamer autoplugged, once we're playing.
    fn log_elements(&self) {
        let Some(bin) = self.playbin.downcast_ref::<gst::Bin>() else { return };
        let mut decoders = Vec::new();
        let mut audio = Vec::new();
        for el in bin.iterate_recurse().into_iter().flatten() {
            let Some(f) = el.factory() else { continue };
            let klass = f.metadata(gst::ELEMENT_METADATA_KLASS).unwrap_or_default();
            if klass.contains("Decoder") {
                decoders.push(f.name().to_string());
            } else if klass.contains("Sink") && klass.contains("Audio") || f.name() == "fakesink" {
                audio.push(f.name().to_string());
            }
        }
        let size = self.frame_size().map_or("?".into(), |(w, h)| format!("{w}x{h}"));
        let convert = if bin.iterate_recurse().into_iter().flatten().any(|e| e.name().starts_with("cudaconvertscale")) {
            "CUDA"
        } else {
            "CPU"
        };
        log::info!("video: playing ({size}, {convert} convert), decoders {decoders:?}, audio sink {audio:?}");
    }

    /// Moves the pipeline to the state the user wants, unless buffering holds it paused.
    fn apply_state(&mut self) {
        let target = if self.want_playing && self.buffering.is_none() { gst::State::Playing } else { gst::State::Paused };
        if self.prerolled && self.pending_seek.is_none() {
            let _ = self.playbin.set_state(target);
        }
    }

    pub fn is_playing(&self) -> bool {
        self.want_playing
    }

    pub fn set_playing(&mut self, on: bool) {
        if on && self.ended {
            self.ended = false;
            self.seek(0.0, true);
        }
        self.want_playing = on;
        self.apply_state();
    }

    /// Seeks to `t` seconds; keyframe-only (fast) unless `accurate`.
    pub fn seek(&mut self, t: f64, accurate: bool) {
        let t = t.clamp(0.0, self.duration.unwrap_or(f64::MAX));
        self.ended = false;
        self.seek_target = Some(t);
        if self.prerolled {
            self.seek_now(t, accurate);
        } else {
            self.pending_seek = Some((t, accurate));
        }
    }

    fn seek_now(&mut self, t: f64, accurate: bool) {
        let precision = if accurate { gst::SeekFlags::ACCURATE } else { gst::SeekFlags::KEY_UNIT };
        let pos = gst::ClockTime::from_nseconds((t * 1e9) as u64);
        if let Err(e) = self.playbin.seek_simple(gst::SeekFlags::FLUSH | precision, pos) {
            log::warn!("video: seek failed: {e}");
        }
        // The flush prerolls again; AsyncDone resumes playing.
        self.prerolled = false;
        self.seek_target = Some(t);
    }

    pub fn set_mute(&self, muted: bool) {
        self.playbin.set_property("mute", muted);
    }

    pub fn set_volume(&self, v: f64) {
        self.playbin.set_property("volume", cubic(v));
    }

    pub fn position(&self) -> Option<f64> {
        self.seek_target.or_else(|| self.playbin.query_position::<gst::ClockTime>().map(|t| t.seconds_f64()))
    }

    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// The pipeline got to PLAYING at least once.
    pub fn reached_playing(&self) -> bool {
        self.logged
    }

    /// True while waiting on the network: before the first frame, or the queue ran dry.
    pub fn buffering(&self) -> bool {
        self.buffering.is_some() || (self.frames == 0 && !self.failed)
    }

    fn frame_size(&self) -> Option<(u32, u32)> {
        let s = self.slot.lock().ok()?;
        (s.w > 0).then_some((s.w, s.h))
    }

    /// Hands a new frame (if one arrived since last time) to `f` as (w, h, rgba).
    pub fn take_frame(&mut self, f: impl FnOnce(u32, u32, &[u8])) -> bool {
        let Ok(mut s) = self.slot.lock() else { return false };
        if !s.fresh {
            return false;
        }
        s.fresh = false;
        self.frames += 1;
        self.last_pts = s.pts;
        f(s.w, s.h, &s.rgba);
        true
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(bus) = self.playbin.bus() {
            bus.unset_sync_handler();
        }
        // Going to Null can block on network threads; don't stall the UI for it.
        let playbin = self.playbin.clone();
        std::thread::spawn(move || {
            let _ = playbin.set_state(gst::State::Null);
            let alive = ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
            log::info!("video: pipeline torn down ({alive} alive)");
        });
    }
}

/// Copies a sample into the shared slot, dropping row padding.
fn copy_frame(sample: &gst::Sample, slot: &Mutex<Slot>) -> Option<()> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let buffer = sample.buffer()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (w, h) = (frame.width(), frame.height());
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let row = w as usize * 4;
    let mut s = slot.lock().ok()?;
    s.rgba.resize(row * h as usize, 0);
    for y in 0..h as usize {
        s.rgba[y * row..(y + 1) * row].copy_from_slice(data.get(y * stride..y * stride + row)?);
    }
    (s.w, s.h) = (w, h);
    s.pts = buffer.pts().map_or(0.0, |t| t.seconds_f64());
    s.fresh = true;
    Some(())
}

/// Slider position → linear amplitude; a cubic curve sounds even across the range.
fn cubic(v: f64) -> f64 {
    v.clamp(0.0, 1.0).powi(3)
}
