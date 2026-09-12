use crate::notifications::{NotificationSoundRequest, SoundDecision};
use libpulse_binding as pulse;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlagSet, SeekMode, State as StreamState, Stream};
use std::f32::consts::PI;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const DURATION_MS: u32 = 180;
pub const PEAK_AMPLITUDE: i16 = 6_553;

#[derive(Clone)]
pub struct NotificationSoundSender {
    sender: SyncSender<NotificationSoundRequest>,
}

impl NotificationSoundSender {
    pub fn try_play(&self, request: NotificationSoundRequest) -> bool {
        try_submit(&self.sender, request)
    }

    pub fn try_decision(&self, decision: SoundDecision) -> bool {
        match decision {
            SoundDecision::Silent => false,
            SoundDecision::Play(request) => self.try_play(request),
        }
    }
}

pub struct NotificationSoundBridge {
    sender: Option<SyncSender<NotificationSoundRequest>>,
    worker: Option<JoinHandle<()>>,
}

impl NotificationSoundBridge {
    pub fn spawn() -> Option<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("xbar-notification-sound".into())
            .spawn(move || run_worker(receiver))
        {
            Ok(worker) => worker,
            Err(error) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: notification sound worker unavailable: {error}");
                }
                return None;
            }
        };
        Some(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub fn sender(&self) -> Option<NotificationSoundSender> {
        self.sender
            .as_ref()
            .cloned()
            .map(|sender| NotificationSoundSender { sender })
    }
}

impl Drop for NotificationSoundBridge {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn try_submit(
    sender: &SyncSender<NotificationSoundRequest>,
    request: NotificationSoundRequest,
) -> bool {
    match sender.try_send(request) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
}

fn run_worker(receiver: mpsc::Receiver<NotificationSoundRequest>) {
    let pcm = default_pcm();
    let mut backend = None;
    while let Ok(_request) = receiver.recv() {
        if backend.is_none() {
            backend = PulseBackend::connect();
        }
        let Some(current) = backend.as_mut() else {
            continue;
        };
        if !current.play(&pcm) {
            backend = None;
        }
    }
}

struct PulseBackend {
    mainloop: Mainloop,
    context: Context,
}

impl PulseBackend {
    fn connect() -> Option<Self> {
        let mut mainloop = Mainloop::new()?;
        let mut proplist = Proplist::new()?;
        let _ = proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, "xbar");
        let mut context =
            Context::new_with_proplist(&mainloop, "xbar-notification-sound", &proplist)?;
        context.connect(None, ContextFlagSet::NOFLAGS, None).ok()?;
        loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) if context.get_state() == ContextState::Ready => {
                    return Some(Self { mainloop, context });
                }
                IterateResult::Success(_) => {
                    if matches!(
                        context.get_state(),
                        ContextState::Failed | ContextState::Terminated
                    ) {
                        return None;
                    }
                }
                IterateResult::Err(_) | IterateResult::Quit(_) => return None,
            }
        }
    }

    fn play(&mut self, pcm: &[u8]) -> bool {
        if self.context.get_state() != ContextState::Ready {
            return false;
        }
        let spec = Spec {
            format: Format::S16NE,
            rate: SAMPLE_RATE,
            channels: CHANNELS,
        };
        let mut stream = match Stream::new(&mut self.context, "xbar notification", &spec, None) {
            Some(stream) => stream,
            None => return false,
        };
        if stream
            .connect_playback(None, None, StreamFlagSet::NOFLAGS, None, None)
            .is_err()
        {
            return false;
        }
        loop {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => match stream.get_state() {
                    StreamState::Ready => break,
                    StreamState::Failed | StreamState::Terminated => return false,
                    _ => {}
                },
                IterateResult::Err(_) | IterateResult::Quit(_) => return false,
            }
        }
        if stream.write(pcm, None, 0, SeekMode::Relative).is_err() {
            return false;
        }
        let operation = stream.drain(None);
        while operation.get_state() == pulse::operation::State::Running {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Err(_) | IterateResult::Quit(_) => return false,
            }
        }
        stream.disconnect().is_ok()
    }
}

pub fn default_pcm() -> Vec<u8> {
    let samples = (SAMPLE_RATE as usize * DURATION_MS as usize) / 1_000;
    let attack = SAMPLE_RATE as usize * 10 / 1_000;
    let release = attack;
    let mut pcm = Vec::with_capacity(samples * 2);
    for index in 0..samples {
        let t = index as f32 / SAMPLE_RATE as f32;
        let tone = (2.0 * PI * 660.0 * t).sin() * 0.6 + (2.0 * PI * 880.0 * t).sin() * 0.4;
        let envelope = if index < attack {
            index as f32 / attack as f32
        } else if index >= samples.saturating_sub(release) {
            samples.saturating_sub(1).saturating_sub(index) as f32
                / release.saturating_sub(1) as f32
        } else {
            1.0
        };
        let value = (tone * envelope * f32::from(PEAK_AMPLITUDE)).round() as i32;
        let value = value.clamp(i32::from(-PEAK_AMPLITUDE), i32::from(PEAK_AMPLITUDE)) as i16;
        pcm.extend_from_slice(&value.to_ne_bytes());
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pcm_is_deterministic_bounded_and_enveloped() {
        let first = default_pcm();
        let second = default_pcm();
        assert_eq!(first, second);
        assert_eq!(
            first.len(),
            (SAMPLE_RATE as usize * DURATION_MS as usize / 1_000) * 2
        );
        let values = first
            .chunks_exact(2)
            .map(|bytes| i16::from_ne_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        assert!(values.iter().all(|value| value.abs() <= PEAK_AMPLITUDE));
        assert!(values[0].abs() <= 1);
        assert!(values.last().is_some_and(|value| value.abs() <= 1));
    }

    #[test]
    fn bounded_submission_accepts_one_pending_request_and_drops_the_next() {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(try_submit(&sender, NotificationSoundRequest::Default));
        assert!(!try_submit(&sender, NotificationSoundRequest::Default));
        drop(receiver);
        assert!(!try_submit(&sender, NotificationSoundRequest::Default));
    }

    #[test]
    fn sound_sender_submits_play_once() {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(try_submit(
            &sender,
            NotificationSoundRequest::Named("message-new".into())
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(NotificationSoundRequest::Named(_))
        ));
    }

    #[test]
    fn silent_decision_submits_nothing() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let sender = NotificationSoundSender { sender };
        assert!(!sender.try_decision(SoundDecision::Silent));
        assert!(receiver.try_recv().is_err());
    }
}
