//! An audio file, decoded in pure Rust and played by the system (upstream's `FilePreview/AudioPlayer.tsx`). Symphonia gives
//! the samples, with `opus-pure` for the one codec it lacks: the waveform's peaks, and a plain WAV for `AVAudioPlayer`, which
//! is what makes seeking exact whatever the original container was. On a Mac, what neither can read goes to `AVAudioFile`,
//! the decoder `decodeAudioData` is upstream, with the same waveform. What that refuses too is handed to the system player as
//! it is, and what the player refuses says so.

use crate::format::{data_url_bytes, format_bytes, format_duration};
use crate::previews::notice;
use crate::widgets::{tool, tool_pressed};
use mewrk_client::prelude::*;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudio {
    /// Interleaved.
    pub samples: Vec<f32>,
    pub channels: u16,
    pub rate: u32,
}

impl DecodedAudio {
    pub fn duration(&self) -> f64 {
        self.samples.len() as f64 / self.channels.max(1) as f64 / self.rate.max(1) as f64
    }
}

/// Decodes a whole file, or says it cannot. A corrupt packet in the middle is skipped, the way a player skips one.
pub fn decode_audio(bytes: Vec<u8>, extension: &str) -> Option<DecodedAudio> {
    if is_ogg_opus(&bytes) {
        return decode_ogg_opus(&bytes);
    }
    decode_with_symphonia(bytes.clone(), extension).or_else(|| system_decode_audio(&bytes, extension))
}

/// What only the system reads: Opus in a CAF, say, which is what `afconvert` writes. `AVAudioFile` takes a path and not bytes.
#[cfg(target_os = "macos")]
fn system_decode_audio(bytes: &[u8], extension: &str) -> Option<DecodedAudio> {
    use objc2::AnyThread;
    use objc2_avf_audio::{AVAudioFile, AVAudioPCMBuffer};
    use objc2_foundation::{NSString, NSURL};
    let extension: String = extension.chars().filter(char::is_ascii_alphanumeric).collect();
    let path = std::env::temp_dir().join(format!("mewrk-audio-{}-{:x}.{extension}", std::process::id(), fluent::text::hash_str(&format!("{:p}{}", bytes.as_ptr(), bytes.len()))));
    std::fs::write(&path, bytes).ok()?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    // SAFETY: the objects are made and used here on one thread; the channel planes are read within the length the buffer
    // reports, and only as the deinterleaved floats `processingFormat` is documented to be, which is checked first.
    let decoded = unsafe {
        (|| {
            let file = AVAudioFile::initForReading_error(AVAudioFile::alloc(), &url).ok()?;
            let format = file.processingFormat();
            let frames = u32::try_from(file.length()).ok().filter(|frames| *frames > 0)?;
            let buffer = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &format, frames)?;
            file.readIntoBuffer_error(&buffer).ok()?;
            let channels = format.channelCount() as usize;
            let planes = buffer.floatChannelData();
            if planes.is_null() || channels == 0 || format.isInterleaved() {
                return None;
            }
            let length = buffer.frameLength() as usize;
            let mut samples = vec![0.0f32; length * channels];
            for channel in 0..channels {
                let plane = std::slice::from_raw_parts((*planes.add(channel)).as_ptr(), length);
                for (frame, sample) in plane.iter().enumerate() {
                    samples[frame * channels + channel] = *sample;
                }
            }
            (!samples.is_empty()).then_some(DecodedAudio { samples, channels: channels as u16, rate: format.sampleRate() as u32 })
        })()
    };
    let _ = std::fs::remove_file(&path);
    decoded
}

#[cfg(not(target_os = "macos"))]
fn system_decode_audio(_bytes: &[u8], _extension: &str) -> Option<DecodedAudio> {
    None
}

fn decode_with_symphonia(bytes: Vec<u8>, extension: &str) -> Option<DecodedAudio> {
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe().format(&hint, source, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    let mut format = probed.format;
    let track = format.default_track()?.clone();
    if track.codec_params.codec == CODEC_TYPE_OPUS {
        return decode_opus_track(format.as_mut(), &track);
    }
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).ok()?;
    let mut samples: Vec<f32> = Vec::new();
    let (mut channels, mut rate) = (0u16, track.codec_params.sample_rate.unwrap_or(0));
    let mut buffer: Option<SampleBuffer<f32>> = None;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track.id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        channels = spec.channels.count() as u16;
        rate = spec.rate;
        let needed = decoded.capacity() as u64;
        if buffer.as_ref().is_none_or(|b| (b.capacity() as u64) < needed * channels as u64) {
            buffer = Some(SampleBuffer::new(needed, spec));
        }
        let b = buffer.as_mut()?;
        b.copy_interleaved_ref(decoded);
        samples.extend_from_slice(b.samples());
    }
    (!samples.is_empty() && channels > 0 && rate > 0).then_some(DecodedAudio { samples, channels, rate })
}

/// Opus decodes to any rate it is asked for, and every file's own is this one's whole-number multiple.
const OPUS_RATE: i32 = 48_000;

/// Whether an Ogg file's first page carries an Opus stream: `OggS`, its 27-byte header and segment table, then `OpusHead`.
fn is_ogg_opus(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS") && bytes.get(26).is_some_and(|segments| bytes.get(27 + *segments as usize..).is_some_and(|body| body.starts_with(b"OpusHead")))
}

/// An `.opus` file, or an Ogg holding Opus. RFC 7845 says where the audio really begins and ends, so this reads the container
/// with `opus-pure`'s own reader rather than Symphonia's: it takes the encoder's delay off the front and the end-trim off the
/// back, and the waveform and the seek bar are exactly as long as the recording.
fn decode_ogg_opus(bytes: &[u8]) -> Option<DecodedAudio> {
    use opus_pure::{OggOpusReader, OpusMSDecoder, Trim, MAX_PACKET_SAMPLES};
    let mut reader = OggOpusReader::new(std::io::Cursor::new(bytes)).ok()?;
    let head = reader.head().clone();
    let channels = head.channel_count as usize;
    enum Either {
        Plain(opus_pure::OpusDecoder),
        Surround(OpusMSDecoder),
    }
    let mut decoder = if head.mapping_family == 0 {
        Either::Plain(head.decoder(OPUS_RATE).ok()?)
    } else {
        Either::Surround(OpusMSDecoder::new(OPUS_RATE, channels, head.mapping_family).ok()?)
    };
    let mut trim = Trim::new(&head, OPUS_RATE, channels).ok()?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();
    for packet in reader.packets() {
        // A page that fails its checksum ends what can be read; what came before it is still the recording.
        let Ok(packet) = packet else { break };
        let decoded = match &mut decoder {
            Either::Plain(d) => d.decode(&packet.data, MAX_PACKET_SAMPLES, &mut block),
            Either::Surround(d) => d.decode(&packet.data, MAX_PACKET_SAMPLES, &mut block),
        };
        let Ok(frames) = decoded else { continue };
        samples.extend_from_slice(trim.keep(&packet, &block[..frames * channels]));
    }
    (!samples.is_empty()).then_some(DecodedAudio { samples, channels: channels as u16, rate: OPUS_RATE as u32 })
}

/// Opus in a container Symphonia can open but `opus-pure` has no reader for: WebM's `.weba`. The stream's `OpusHead` is the
/// track's extra data there, which is where its channels and encoder delay are.
fn decode_opus_track(format: &mut dyn symphonia::core::formats::FormatReader, track: &symphonia::core::formats::Track) -> Option<DecodedAudio> {
    use opus_pure::{OpusDecoder, MAX_PACKET_SAMPLES};
    let head = track.codec_params.extra_data.as_deref().filter(|h| h.starts_with(b"OpusHead"));
    let channels = head.and_then(|h| h.get(9).copied()).map(usize::from).or_else(|| track.codec_params.channels.map(|c| c.count())).unwrap_or(2);
    // Surround here would need the mapping table a WebM file seldom carries right; it goes to the system player instead.
    if !(1..=2).contains(&channels) {
        return None;
    }
    let pre_skip = head.and_then(|h| h.get(10..12)).map_or(0, |b| u16::from_le_bytes([b[0], b[1]])) as usize;
    let mut decoder = OpusDecoder::new(OPUS_RATE, channels).ok()?;
    decoder.gain_q8 = head.and_then(|h| h.get(16..18)).map_or(0, |b| i32::from(i16::from_le_bytes([b[0], b[1]])));
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track.id {
            continue;
        }
        let Ok(frames) = decoder.decode(&packet.data, MAX_PACKET_SAMPLES, &mut block) else { continue };
        samples.extend_from_slice(&block[..frames * channels]);
    }
    samples.drain(..(pre_skip * channels).min(samples.len()));
    (!samples.is_empty()).then_some(DecodedAudio { samples, channels: channels as u16, rate: OPUS_RATE as u32 })
}

/// 16-bit PCM in a RIFF container: the one form every system player takes.
pub fn encode_wav(audio: &DecodedAudio) -> Vec<u8> {
    let data_len = audio.samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    let block = audio.channels * 2;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&audio.channels.to_le_bytes());
    out.extend_from_slice(&audio.rate.to_le_bytes());
    out.extend_from_slice(&(audio.rate * block as u32).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in &audio.samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

/// The loudest sample in each of `buckets` equal slices, across every channel. Striding keeps a long recording from
/// costing a full pass; the eye cannot tell a peak found in every fourth sample from the true one.
pub fn peaks(audio: &DecodedAudio, buckets: usize) -> Vec<f32> {
    let channels = audio.channels.max(1) as usize;
    let frames = audio.samples.len() / channels;
    let size = (frames / buckets.max(1)).max(1);
    let mut result = vec![0.0f32; buckets];
    for (bucket, slot) in result.iter_mut().enumerate() {
        let (start, end) = (bucket * size, ((bucket + 1) * size).min(frames));
        let mut at = start;
        while at < end {
            for c in 0..channels {
                *slot = slot.max(audio.samples[at * channels + c].abs());
            }
            at += 4;
        }
    }
    result
}

#[cfg(target_os = "macos")]
mod output {
    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_avf_audio::AVAudioPlayer;
    use objc2_foundation::NSData;

    pub struct Player(Retained<AVAudioPlayer>);

    impl Player {
        pub fn new(bytes: &[u8]) -> Option<Self> {
            let data = NSData::with_bytes(bytes);
            // SAFETY: the data is a valid NSData; the player answers an error for bytes it cannot read.
            let player = unsafe { AVAudioPlayer::initWithData_error(AVAudioPlayer::alloc(), &data) }.ok()?;
            // SAFETY: plain call on the object just made.
            unsafe { player.prepareToPlay() };
            Some(Self(player))
        }

        pub fn play(&self) {
            // SAFETY: plain call on an object this struct owns.
            unsafe { self.0.play() };
        }

        pub fn pause(&self) {
            // SAFETY: as above.
            unsafe { self.0.pause() };
        }

        pub fn is_playing(&self) -> bool {
            // SAFETY: plain property read.
            unsafe { self.0.isPlaying() }
        }

        pub fn position(&self) -> f64 {
            // SAFETY: plain property read.
            unsafe { self.0.currentTime() }
        }

        pub fn seek(&self, to: f64) {
            // SAFETY: plain property write.
            unsafe { self.0.setCurrentTime(to) };
        }

        pub fn duration(&self) -> f64 {
            // SAFETY: plain property read.
            unsafe { self.0.duration() }
        }

        pub fn channels(&self) -> usize {
            // SAFETY: plain property read.
            unsafe { self.0.numberOfChannels() }
        }

        pub fn set_volume(&self, level: f32) {
            // SAFETY: plain property write.
            unsafe { self.0.setVolume(level) };
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            // SAFETY: plain call on an object this struct owns.
            unsafe { self.0.stop() };
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod output {
    /// No system player is reached from here on other platforms: the card says so and offers nothing to press.
    pub struct Player;

    impl Player {
        pub fn new(_bytes: &[u8]) -> Option<Self> {
            None
        }
        pub fn play(&self) {}
        pub fn pause(&self) {}
        pub fn is_playing(&self) -> bool {
            false
        }
        pub fn position(&self) -> f64 {
            0.0
        }
        pub fn seek(&self, _to: f64) {}
        pub fn duration(&self) -> f64 {
            0.0
        }
        pub fn channels(&self) -> usize {
            0
        }
        pub fn set_volume(&self, _level: f32) {}
    }
}

pub use output::Player;

enum Stage {
    Decoding,
    Ready { audio: Option<Arc<DecodedAudio>>, player: Player },
    Failed,
}

struct AudioState {
    stage: Stage,
    playing: bool,
    position: f64,
    peak_cache: Option<(usize, Rc<Vec<f32>>)>,
    started: bool,
}

/// The reader's volume, kept so that opening another file keeps it.
#[derive(Clone, Copy)]
struct Level {
    volume: f32,
    muted: bool,
}

impl Default for Level {
    fn default() -> Self {
        Self { volume: 1.0, muted: false }
    }
}

fn level_now(state: &Local<Level>) -> f32 {
    let l = state.get();
    if l.muted { 0.0 } else { l.volume }
}

/// The card: title, waveform, controls and what is known of the file.
pub fn audio_player(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>) -> E {
    let t_ = theme();
    let hash = fluent::text::hash_str(source);
    let state = use_state(("audio", key, hash), || AudioState { stage: Stage::Decoding, playing: false, position: 0.0, peak_cache: None, started: false });
    let level = use_state(("audio-level", key), Level::default);
    if !state.with(|s| s.started) {
        state.update(|s| s.started = true);
        let (source, extension) = (source.clone(), name.rsplit('.').next().unwrap_or("").to_lowercase());
        let target = state.clone();
        // The decode may take a moment, and a `Local` stays on this thread: the answer waits in a slot the UI thread polls.
        let slot: std::sync::Arc<std::sync::Mutex<Option<(Option<DecodedAudio>, Vec<u8>)>>> = Default::default();
        let writer = slot.clone();
        app.spawn(move |task| {
            let raw = data_url_bytes(&source).unwrap_or_default();
            let decoded = decode_audio(raw.clone(), &extension);
            *writer.lock().unwrap_or_else(|p| p.into_inner()) = Some((decoded, raw));
            task.post(|_| repaint());
        });
        DECODES.with(|d| d.borrow_mut().push((target, slot)));
    }
    DECODES.with(|d| {
        d.borrow_mut().retain(|(target, slot)| {
            let Some((decoded, raw)) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() else { return true };
            let wav = decoded.as_ref().map(encode_wav);
            let player = Player::new(wav.as_deref().unwrap_or(&raw));
            target.update(|s| {
                s.stage = match (decoded, player) {
                    (decoded, Some(player)) => {
                        player.set_volume(level_now(&level));
                        Stage::Ready { audio: decoded.map(Arc::new), player }
                    }
                    _ => Stage::Failed,
                };
            });
            false
        });
    });
    if state.with(|s| matches!(s.stage, Stage::Failed)) {
        return notice(t("当前界面引擎无法解码这种音频格式。", "This window's engine cannot decode this audio format."));
    }
    let (ready, playing, position, duration, channels) = state.with(|s| match &s.stage {
        Stage::Ready { audio, player } => (true, s.playing, s.position, audio.as_ref().map_or_else(|| player.duration(), |a| a.duration()), audio.as_ref().map_or_else(|| player.channels(), |a| a.channels as usize)),
        _ => (false, false, 0.0, 0.0, 0),
    });

    let play_toggle = {
        let state = state.clone();
        Rc::new(move || state.update(|s| {
            let at = s.position;
            let Stage::Ready { player, .. } = &s.stage else { return };
            if s.playing {
                player.pause();
                s.playing = false;
            } else {
                if at >= player.duration() {
                    player.seek(0.0);
                    s.position = 0.0;
                }
                player.play();
                s.playing = true;
            }
        }))
    };
    let seek = {
        let state = state.clone();
        Rc::new(move |to: f64| state.update(|s| {
            let Stage::Ready { player, .. } = &s.stage else { return };
            let clamped = to.clamp(0.0, player.duration());
            player.seek(clamped);
            s.position = clamped;
        }))
    };

    // The waveform: every column's peak, drawn in the text colour up to the playhead and faint after it. While playing it
    // also keeps the position, and notices the end.
    let wave_state = state.clone();
    let wave = custom::<App>(move |_app, ui, r| {
        let theme_now = ui.theme;
        let (values, progress) = wave_state.update(|s| {
            if let Stage::Ready { player, .. } = &s.stage
                && s.playing
            {
                if player.is_playing() {
                    s.position = player.position();
                } else {
                    s.playing = false;
                    s.position = player.duration();
                }
                ui.wake_after(0.03);
            }
            let columns = ((r.w / 3.0).floor() as usize).max(1);
            let (audio, total) = match &s.stage {
                Stage::Ready { audio, player } => (audio.clone(), player.duration()),
                _ => (None, 0.0),
            };
            let values = match (&s.peak_cache, audio) {
                (Some((c, values)), _) if *c == columns => values.clone(),
                (_, Some(audio)) => {
                    let values = Rc::new(peaks(&audio, columns));
                    s.peak_cache = Some((columns, values.clone()));
                    values
                }
                _ => Rc::new(vec![0.02; columns]),
            };
            (values, if total > 0.0 { s.position / total } else { 0.0 })
        });
        let n = values.len() as f64;
        for (column, value) in values.iter().enumerate() {
            let bar = value.clamp(0.02, 1.0) * (r.h - 2.0);
            let color = if (column as f64) / n <= progress { theme_now.text_primary } else { theme_now.text_tertiary.with_alpha(0.45) };
            ui.fill(rect(r.x + column as f32 * 3.0, r.y + (r.h - bar) * 0.5, 2.0, bar), 0.0, color);
        }
    });
    let wave_seek = seek.clone();
    let wave = div()
        .child(wave.w_full().h_full().pass_through())
        .key(("audio-wave", key))
        .h(72.0)
        .shrink0()
        .min_w(0.0)
        .focusable()
        .tooltip(t("播放位置", "Playback position"))
        .aria_value(position.round(), 0.0, duration.round())
        .when(a11y_active(), |e| e.aria_value_text(format_duration(position)))
        .on_pointer(move |_: &mut App, ev| {
            if ev.phase != Phase::Up && duration > 0.0 {
                wave_seek(((ev.x - ev.rect.x) / ev.rect.w.max(1.0)) as f64 * duration);
            }
        })
        .on_key({
            let (seek, toggle) = (seek.clone(), play_toggle.clone());
            move |_: &mut App, k| match k.key {
                Key::Left => {
                    seek(position - 5.0);
                    true
                }
                Key::Right => {
                    seek(position + 5.0);
                    true
                }
                Key::Char(' ') => {
                    toggle();
                    true
                }
                _ => false,
            }
        });

    let restart = {
        let state = state.clone();
        move |_: &mut App| state.update(|s| {
            let Stage::Ready { player, .. } = &s.stage else { return };
            player.seek(0.0);
            player.play();
            s.position = 0.0;
            s.playing = true;
        })
    };
    let apply_level = {
        let (state, level) = (state.clone(), level.clone());
        move || state.with(|s| {
            if let Stage::Ready { player, .. } = &s.stage {
                player.set_volume(level_now(&level));
            }
        })
    };
    let mute = {
        let (level, apply) = (level.clone(), apply_level.clone());
        move |_: &mut App| {
            level.update(|l| l.muted = !l.muted);
            apply();
        }
    };
    let shown_level = level_now(&level);
    let slider = {
        let (level, apply) = (level.clone(), apply_level.clone());
        let (key_level, key_apply) = (level.clone(), apply_level.clone());
        let track = custom::<App>(move |_app, ui, r| {
            let theme_now = ui.theme;
            let y = r.y + r.h * 0.5;
            ui.fill(rect(r.x, y - 1.5, r.w, 3.0), 1.5, theme_now.divider);
            ui.fill(rect(r.x, y - 1.5, r.w * shown_level, 3.0), 1.5, theme_now.accent);
            ui.fill(rect(r.x + r.w * shown_level - 5.0, y - 5.0, 10.0, 10.0), 5.0, theme_now.accent);
        });
        div()
        .child(track.w_full().h_full().pass_through())
        .key(("audio-volume", key))
        .w(90.0)
        .h(20.0)
        .tooltip(t("音量", "Volume"))
        .aria_value(f64::from(shown_level), 0.0, 1.0)
        .on_pointer(move |_: &mut App, ev| {
            if ev.phase != Phase::Up {
                level.set(Level { volume: ((ev.x - ev.rect.x) / ev.rect.w.max(1.0)).clamp(0.0, 1.0), muted: false });
                apply();
            }
        })
        // A range input takes the arrow keys.
        .on_key(move |_: &mut App, k| {
            let step = match k.key {
                Key::Left | Key::Down => -0.05,
                Key::Right | Key::Up => 0.05,
                _ => return false,
            };
            key_level.set(Level { volume: (shown_level + step).clamp(0.0, 1.0), muted: false });
            key_apply();
            true
        })
    };
    let muted = level.get().muted;
    let toggle = play_toggle.clone();
    // In a narrow pane the volume slider drops to its own line rather than squeezing the time.
    let controls = row().wrap().items_center().gap(4.0).shrink0()
        .child(tool(("audio-play", key), if playing { "Pause" } else { "Play" }, if playing { t("暂停", "Pause") } else { t("播放", "Play") }, 26.0, 15.0).bg(t_.subtle_pressed).disabled(!ready).on_click(move |_: &mut App| toggle()))
        .child(tool(("audio-restart", key), "RotateCcw", t("从头播放", "Restart"), 26.0, 13.0).disabled(!ready).on_click(restart))
        .child(text(if ready { format!("{} / {}", format_duration(position), format_duration(duration)) } else { t("正在解码…", "Decoding…").to_owned() }).ty(TypeStyle { mono: true, ..FINE }).color(t_.text_primary).nowrap().px(6.0).flex_grow(1.0).shrink0())
        .child(row().items_center().gap(4.0).ml_auto().child(tool_mute(key, muted).on_click(mute)).child(slider));
    let mut card = col().gap(10.0).p(14.0).radius(10.0).bg(t_.card).border(t_.card_stroke).w_full().max_w(560.0).child(text(name.to_owned()).ty(BODY_SMALL_STRONG).color(t_.text_primary).ellipsis().tooltip(name.to_owned())).child(wave).child(controls);
    if ready {
        let mut meta = tf("{channels} 声道", "{channels} channel(s)", &[("channels", &channels)]);
        if let Some(b) = bytes {
            meta.push_str(" · ");
            meta.push_str(&format_bytes(b));
        }
        card = card.child(text(meta).ty(FINE).color(t_.text_tertiary).nowrap());
    }
    col().flex1().min_h(0.0).min_w(0.0).center().p(20.0).child(card)
}

fn tool_mute(key: &str, muted: bool) -> E {
    tool_pressed(tool(("audio-mute", key), if muted { "VolumeX" } else { "Volume2" }, if muted { t("取消静音", "Unmute") } else { t("静音", "Mute") }, 26.0, 13.0), muted)
}

type DecodeSlot = std::sync::Arc<std::sync::Mutex<Option<(Option<DecodedAudio>, Vec<u8>)>>>;

thread_local! {
    static DECODES: std::cell::RefCell<Vec<(Local<AudioState>, DecodeSlot)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, seconds: f32) -> DecodedAudio {
        let samples = (0..(rate as f32 * seconds) as usize).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        DecodedAudio { samples, channels: 1, rate }
    }

    #[test]
    fn a_wav_round_trips_through_the_decoder() {
        let audio = tone(8000, 0.5);
        let wav = encode_wav(&audio);
        let back = decode_audio(wav, "wav").expect("a wav decodes");
        assert_eq!((back.channels, back.rate), (1, 8000));
        assert!((back.duration() - 0.5).abs() < 0.01);
        assert!(back.samples.iter().map(|s| s.abs()).fold(0.0, f32::max) > 0.45);
    }

    /// A real Ogg Opus file: `opus-pure`'s own encoder over a tone, ended the way RFC 7845 says so it comes back exactly as long.
    fn ogg_opus(seconds: f32, channels: usize) -> Vec<u8> {
        use opus_pure::{Application, OggOpusWriter, OpusEncoder, OpusHead, MAX_PACKET_BYTES};
        let (rate, frame) = (48_000usize, 960usize);
        let total = (rate as f32 * seconds) as usize;
        let mut encoder = OpusEncoder::new(rate as i32, channels, Application::Audio).unwrap();
        let head = OpusHead::for_encoder(&encoder, rate as u32);
        let pre_skip = head.pre_skip as usize;
        let final_granule = (pre_skip + total) as u64;
        let frames = (total + pre_skip).div_ceil(frame);
        let mut writer = OggOpusWriter::new(Vec::new(), head).unwrap();
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        let mut block = vec![0.0f32; frame * channels];
        for i in 0..frames {
            for (n, slot) in block.chunks_exact_mut(channels).enumerate() {
                let at = i * frame + n;
                let value = if at < total { (at as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5 } else { 0.0 };
                slot.fill(value);
            }
            let n = encoder.encode(&block, frame, &mut packet).unwrap();
            if i + 1 == frames {
                let duration = final_granule - writer.granule() as u64;
                writer.write_packet_with_duration(&packet[..n], duration as u32).unwrap();
            } else {
                writer.write_packet(&packet[..n]).unwrap();
            }
        }
        writer.finish().unwrap()
    }

    #[test]
    fn decodes_a_real_opus_file_to_exactly_its_length() {
        for channels in [1, 2] {
            let file = ogg_opus(1.5, channels);
            assert!(is_ogg_opus(&file));
            let audio = decode_audio(file, "opus").expect("an Opus file decodes");
            assert_eq!((audio.channels as usize, audio.rate), (channels, 48_000));
            assert_eq!(audio.samples.len() / channels, 72_000, "the delay and the end-trim are both taken off");
            let loudest = audio.samples.iter().map(|s| s.abs()).fold(0.0, f32::max);
            assert!(loudest > 0.3 && loudest < 0.8, "{loudest}");
            let p = peaks(&audio, 30);
            assert!(p.iter().all(|v| *v > 0.2), "a steady tone fills every column: {p:?}");
        }
    }

    #[test]
    fn reads_an_opus_stream_whatever_its_extension_says() {
        // `.ogg`, or no helpful name at all: the first page says what it is.
        assert!(decode_audio(ogg_opus(0.2, 1), "ogg").is_some());
        assert!(decode_audio(ogg_opus(0.2, 1), "bin").is_some());
    }

    #[test]
    fn keeps_what_came_before_the_damage_in_an_opus_file() {
        let mut file = ogg_opus(2.0, 1);
        let length = file.len();
        file[length * 3 / 4] ^= 0xff;
        let audio = decode_audio(file, "opus").expect("the pages before it");
        assert!(audio.duration() > 0.5, "{}", audio.duration());
    }

    /// A real WebM (`.weba`) holding Opus packets: an EBML header, and a Segment with one audio track and a Cluster.
    fn webm_opus(seconds: f32) -> Vec<u8> {
        use opus_pure::{Application, OpusEncoder, OpusHead, MAX_PACKET_BYTES};
        fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
            let mut out = id.to_vec();
            // An EBML size in its longest form: a marker byte, then seven of length.
            out.push(0x01);
            out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
            out.extend_from_slice(body);
            out
        }
        let mut encoder = OpusEncoder::new(48_000, 1, Application::Audio).unwrap();
        let pre_skip = OpusHead::for_encoder(&encoder, 48_000).pre_skip;
        let mut private = b"OpusHead\x01\x01".to_vec();
        private.extend_from_slice(&pre_skip.to_le_bytes());
        private.extend_from_slice(&48_000u32.to_le_bytes());
        private.extend_from_slice(&[0, 0, 0]);
        let header = element(&[0x1a, 0x45, 0xdf, 0xa3], &element(&[0x42, 0x82], b"webm"));
        let audio = element(&[0xe1], &[element(&[0xb5], &48_000f64.to_be_bytes()), element(&[0x9f], &[1])].concat());
        let track = [element(&[0xd7], &[1]), element(&[0x73, 0xc5], &[1]), element(&[0x83], &[2]), element(&[0x86], b"A_OPUS"), element(&[0x63, 0xa2], &private), audio].concat();
        let tracks = element(&[0x16, 0x54, 0xae, 0x6b], &element(&[0xae], &track));
        let info = element(&[0x15, 0x49, 0xa9, 0x66], &element(&[0x2a, 0xd7, 0xb1], &1_000_000u32.to_be_bytes()));
        let mut cluster = element(&[0xe7], &[0]);
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        let frames = (48_000.0 * seconds) as usize / 960;
        for i in 0..frames {
            let pcm: Vec<f32> = (0..960).map(|n| ((i * 960 + n) as f32 * 0.0576).sin() * 0.5).collect();
            let n = encoder.encode(&pcm, 960, &mut packet).unwrap();
            let mut block = vec![0x81];
            block.extend_from_slice(&((i * 20) as i16).to_be_bytes());
            block.push(0x80);
            block.extend_from_slice(&packet[..n]);
            cluster.extend_from_slice(&element(&[0xa3], &block));
        }
        let segment = element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks, element(&[0x1f, 0x43, 0xb6, 0x75], &cluster)].concat());
        [header, segment].concat()
    }

    #[test]
    fn decodes_opus_in_a_webm_file() {
        let audio = decode_audio(webm_opus(1.0), "weba").expect("WebM with Opus decodes");
        assert_eq!((audio.channels, audio.rate), (1, 48_000));
        assert!((audio.duration() - 1.0).abs() < 0.05, "{}", audio.duration());
        assert!(audio.samples.iter().map(|s| s.abs()).fold(0.0, f32::max) > 0.3);
    }

    #[test]
    fn peaks_find_the_loudest_sample_of_each_column() {
        let audio = DecodedAudio { samples: vec![0.1, 0.1, 0.9, 0.1, 0.1, 0.1, 0.1, 0.1], channels: 1, rate: 8 };
        let p = peaks(&audio, 2);
        assert_eq!(p.len(), 2);
        assert!((p[0] - 0.1).abs() < 0.01 || (p[0] - 0.9).abs() < 0.01);
    }

    #[test]
    fn says_nothing_of_bytes_that_are_not_audio() {
        assert!(decode_audio(b"not audio".to_vec(), "mp3").is_none());
    }

    /// Apple's own Opus encoder, which writes it in a CAF: a stream nothing here made, read two ways. Symphonia's container and
    /// `opus-pure`'s decoder on one side, the system's whole path, the fallback, on the other; they have to agree.
    #[cfg(target_os = "macos")]
    #[test]
    fn reads_an_opus_stream_from_another_encoder_as_the_system_does() {
        let dir = std::env::temp_dir().join(format!("mewrk-caf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (wav, caf) = (dir.join("tone.wav"), dir.join("tone.caf"));
        std::fs::write(&wav, encode_wav(&tone(48_000, 1.0))).unwrap();
        let made = std::process::Command::new("afconvert").args(["-f", "caff", "-d", "opus", "-b", "64000"]).arg(&wav).arg(&caf).status();
        if !made.is_ok_and(|status| status.success()) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let bytes = std::fs::read(&caf).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let ours = decode_audio(bytes.clone(), "caf").expect("Symphonia opens it and opus-pure decodes it");
        let system = system_decode_audio(&bytes, "caf").expect("the system reads its own file");
        for audio in [&ours, &system] {
            assert_eq!((audio.channels, audio.rate), (1, 48_000));
            assert!((audio.duration() - 1.0).abs() < 0.1, "{}", audio.duration());
        }
        let loudest = |audio: &DecodedAudio| audio.samples.iter().map(|s| s.abs()).fold(0.0, f32::max);
        assert!((loudest(&ours) - loudest(&system)).abs() < 0.1 && loudest(&ours) > 0.3, "{} {}", loudest(&ours), loudest(&system));
        let player = Player::new(&encode_wav(&system)).expect("what it decoded to is a plain WAV");
        player.seek(0.5);
        assert!((player.position() - 0.5).abs() < 0.01);
    }

    /// iLBC is a codec Symphonia has nothing for, and the system does.
    #[cfg(target_os = "macos")]
    #[test]
    fn decodes_what_only_the_system_reads_with_the_same_waveform() {
        let dir = std::env::temp_dir().join(format!("mewrk-ilbc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (wav, caf) = (dir.join("tone.wav"), dir.join("tone.caf"));
        std::fs::write(&wav, encode_wav(&tone(8_000, 1.0))).unwrap();
        let made = std::process::Command::new("afconvert").args(["-f", "caff", "-d", "ilbc"]).arg(&wav).arg(&caf).status();
        if !made.is_ok_and(|status| status.success()) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let bytes = std::fs::read(&caf).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(decode_with_symphonia(bytes.clone(), "caf").is_none(), "this is what the fallback is for");
        let audio = decode_audio(bytes, "caf").expect("the system reads it");
        assert!((audio.duration() - 1.0).abs() < 0.1, "{}", audio.duration());
        assert!(!peaks(&audio, 10).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_opus_file_plays_and_seeks_like_any_other() {
        let audio = decode_audio(ogg_opus(2.0, 2), "opus").unwrap();
        let player = Player::new(&encode_wav(&audio)).expect("what it decoded to is a plain WAV");
        assert!((player.duration() - 2.0).abs() < 0.01);
        assert_eq!(player.channels(), 2);
        player.seek(1.25);
        assert!((player.position() - 1.25).abs() < 0.01, "{}", player.position());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_player_takes_the_wav_and_knows_its_length() {
        let wav = encode_wav(&tone(8000, 1.0));
        let player = Player::new(&wav).expect("AVAudioPlayer reads a plain WAV");
        assert!((player.duration() - 1.0).abs() < 0.05);
        assert_eq!(player.channels(), 1);
        player.seek(0.5);
        assert!((player.position() - 0.5).abs() < 0.05);
    }
}
