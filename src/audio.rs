use crate::core::{AudioDevice, AudioState, Event};
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::subscribe::{Facility, InterestMaskSet, Operation};
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::mainloop::{api::Mainloop as MainloopTrait, events::io::FlagSet as IoFlagSet};
use pulse::proplist::Proplist;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type EventQueue = Arc<Mutex<VecDeque<Event>>>;

pub struct AudioBridge {
    reader: UnixStream,
    events: EventQueue,
    command_writer: Mutex<UnixStream>,
    _thread: JoinHandle<()>,
}

#[derive(Default)]
struct RefreshFlags {
    server: AtomicBool,
    sink: AtomicBool,
    source: AtomicBool,
    inventory_sinks: AtomicBool,
    inventory_sources: AtomicBool,
    default_sink: Mutex<Option<String>>,
    default_source: Mutex<Option<String>>,
    channels: Mutex<u8>,
    source_channels: Mutex<u8>,
    muted: AtomicBool,
    source_muted: AtomicBool,
    volume: AtomicU32,
    source_volume: AtomicU32,
    output_description: Mutex<Option<String>>,
    input_description: Mutex<Option<String>>,
    outputs: Mutex<Vec<AudioDevice>>,
    inputs: Mutex<Vec<AudioDevice>>,
    inventory_sinks_done: AtomicBool,
    inventory_sources_done: AtomicBool,
}

#[derive(Clone)]
enum AudioCommand {
    SetOutputVolume(u32),
    ToggleOutputMute,
    SetInputVolume(u32),
    ToggleInputMute,
    SetDefaultOutput(String),
    SetDefaultInput(String),
}

fn refresh_detail_for(operation: Option<Operation>) -> bool {
    operation != Some(Operation::Removed)
}

impl AudioBridge {
    pub fn start() -> io::Result<Self> {
        let (reader, event_writer) = UnixStream::pair()?;
        let (command_reader, command_writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        event_writer.set_nonblocking(true)?;
        command_reader.set_nonblocking(true)?;
        command_writer.set_nonblocking(true)?;
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let thread_events = Arc::clone(&events);
        let thread = thread::Builder::new()
            .name("xbar-audio".into())
            .spawn(move || run(thread_events, event_writer, command_reader))?;
        Ok(Self {
            reader,
            events,
            command_writer: Mutex::new(command_writer),
            _thread: thread,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }

    fn send_value(&self, opcode: u8, percent: u32) {
        let mut writer = self
            .command_writer
            .lock()
            .expect("audio command lock poisoned");
        let mut command = [0_u8; 5];
        command[0] = opcode;
        command[1..].copy_from_slice(&percent.min(100).to_le_bytes());
        let _ = writer.write_all(&command);
    }

    pub fn set_volume(&self, percent: u32) {
        self.send_value(1, percent);
    }

    pub fn set_input_volume(&self, percent: u32) {
        self.send_value(3, percent);
    }

    pub fn toggle_mute(&self) {
        let _ = self
            .command_writer
            .lock()
            .expect("audio command lock poisoned")
            .write_all(&[2]);
    }

    pub fn toggle_input_mute(&self) {
        let _ = self
            .command_writer
            .lock()
            .expect("audio command lock poisoned")
            .write_all(&[4]);
    }

    fn send_name(&self, opcode: u8, name: &str) {
        let bytes = name.as_bytes();
        if bytes.len() > u16::MAX as usize {
            return;
        }
        let mut command = Vec::with_capacity(3 + bytes.len());
        command.push(opcode);
        command.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        command.extend_from_slice(bytes);
        let _ = self
            .command_writer
            .lock()
            .expect("audio command lock poisoned")
            .write_all(&command);
    }

    pub fn set_default_output(&self, name: &str) {
        self.send_name(5, name);
    }
    pub fn set_default_input(&self, name: &str) {
        self.send_name(6, name);
    }

    pub fn drain_events(&mut self) -> io::Result<Vec<Event>> {
        let mut buffer = [0_u8; 128];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(self
            .events
            .lock()
            .expect("audio event queue poisoned")
            .drain(..)
            .collect())
    }
}

fn publish(events: &EventQueue, writer: &Arc<Mutex<UnixStream>>, event: Event) {
    events
        .lock()
        .expect("audio event queue poisoned")
        .push_back(event);
    let _ = writer.lock().expect("audio wake poisoned").write_all(&[1]);
}

fn run(events: EventQueue, writer: UnixStream, mut command_reader: UnixStream) {
    let writer = Arc::new(Mutex::new(writer));
    let Some(mut mainloop) = Mainloop::new() else {
        publish(&events, &writer, Event::AudioUnavailable);
        return;
    };
    let mut proplist = match Proplist::new() {
        Some(value) => value,
        None => {
            publish(&events, &writer, Event::AudioUnavailable);
            return;
        }
    };
    let _ = proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, "xbar");
    let Some(mut context) = Context::new_with_proplist(&mainloop, "xbar", &proplist) else {
        publish(&events, &writer, Event::AudioUnavailable);
        return;
    };
    if context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .is_err()
    {
        publish(&events, &writer, Event::AudioUnavailable);
        return;
    }
    loop {
        match mainloop.iterate(true) {
            IterateResult::Err(_) | IterateResult::Quit(_) => {
                publish(&events, &writer, Event::AudioUnavailable);
                return;
            }
            IterateResult::Success(_) => {}
        }
        if context.get_state() == ContextState::Ready {
            break;
        }
        if matches!(
            context.get_state(),
            ContextState::Failed | ContextState::Terminated
        ) {
            publish(&events, &writer, Event::AudioUnavailable);
            return;
        }
    }

    let flags = Arc::new(RefreshFlags::default());
    flags.inventory_sinks_done.store(true, Ordering::Release);
    flags.inventory_sources_done.store(true, Ordering::Release);
    let pending_commands = Arc::new(Mutex::new(VecDeque::<AudioCommand>::new()));
    let command_queue = Arc::clone(&pending_commands);
    let _command_io = mainloop.new_io_event(
        command_reader.as_raw_fd(),
        IoFlagSet::INPUT,
        Box::new(move |_, _, _| {
            let mut byte = [0_u8; 1];
            loop {
                match command_reader.read(&mut byte) {
                    Ok(0) => break,
                    Ok(1) => match byte[0] {
                        1 => {
                            let mut value = [0_u8; 4];
                            if command_reader.read_exact(&mut value).is_ok() {
                                command_queue
                                    .lock()
                                    .expect("audio command queue poisoned")
                                    .push_back(AudioCommand::SetOutputVolume(u32::from_le_bytes(
                                        value,
                                    )));
                            }
                        }
                        2 => command_queue
                            .lock()
                            .expect("audio command queue poisoned")
                            .push_back(AudioCommand::ToggleOutputMute),
                        3 => {
                            let mut value = [0_u8; 4];
                            if command_reader.read_exact(&mut value).is_ok() {
                                command_queue
                                    .lock()
                                    .expect("audio command queue poisoned")
                                    .push_back(AudioCommand::SetInputVolume(u32::from_le_bytes(
                                        value,
                                    )));
                            }
                        }
                        4 => command_queue
                            .lock()
                            .expect("audio command queue poisoned")
                            .push_back(AudioCommand::ToggleInputMute),
                        5 | 6 => {
                            let mut length = [0_u8; 2];
                            if command_reader.read_exact(&mut length).is_ok() {
                                let mut name = vec![0_u8; u16::from_le_bytes(length) as usize];
                                if command_reader.read_exact(&mut name).is_ok() {
                                    if let Ok(name) = String::from_utf8(name) {
                                        command_queue
                                            .lock()
                                            .expect("audio command queue poisoned")
                                            .push_back(if byte[0] == 5 {
                                                AudioCommand::SetDefaultOutput(name)
                                            } else {
                                                AudioCommand::SetDefaultInput(name)
                                            });
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                    _ => break,
                }
            }
        }),
    );
    let callback_flags = Arc::clone(&flags);
    context.set_subscribe_callback(Some(Box::new(
        move |facility, operation, index| {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: audio subscription facility={facility:?} operation={operation:?} index={index:?}"
                );
            }
            match facility {
                Some(Facility::Server) => callback_flags.server.store(true, Ordering::Release),
                Some(Facility::Sink) => {
                    callback_flags
                        .inventory_sinks
                        .store(true, Ordering::Release);
                    if refresh_detail_for(operation) {
                        callback_flags.sink.store(true, Ordering::Release);
                    }
                }
                Some(Facility::Source) => {
                    callback_flags
                        .inventory_sources
                        .store(true, Ordering::Release);
                    if refresh_detail_for(operation) {
                        callback_flags.source.store(true, Ordering::Release);
                    }
                }
                _ => {}
            }
        },
    )));
    let _subscription = context.subscribe(
        InterestMaskSet::SINK | InterestMaskSet::SOURCE | InterestMaskSet::SERVER,
        |_| {},
    );

    let server_flags = Arc::clone(&flags);
    let introspector = context.introspect();
    let _server = introspector.get_server_info(move |info| {
        *server_flags
            .default_sink
            .lock()
            .expect("audio sink lock poisoned") =
            info.default_sink_name.as_deref().map(str::to_owned);
        *server_flags
            .default_source
            .lock()
            .expect("audio source lock poisoned") = info
            .default_source_name
            .as_deref()
            .filter(|name| !name.ends_with(".monitor"))
            .map(str::to_owned);
        server_flags.sink.store(true, Ordering::Release);
        server_flags.source.store(true, Ordering::Release);
        server_flags.inventory_sinks.store(true, Ordering::Release);
        server_flags
            .inventory_sources
            .store(true, Ordering::Release);
    });

    loop {
        match mainloop.iterate(true) {
            IterateResult::Err(_) | IterateResult::Quit(_) => break,
            IterateResult::Success(_) => {}
        }
        if context.get_state() != ContextState::Ready {
            publish(&events, &writer, Event::AudioUnavailable);
            break;
        }
        if flags.inventory_sinks.swap(false, Ordering::AcqRel) {
            flags.inventory_sinks_done.store(false, Ordering::Release);
            flags
                .outputs
                .lock()
                .expect("audio outputs lock poisoned")
                .clear();
            let flags_for_callback = Arc::clone(&flags);
            let events_for_callback = Arc::clone(&events);
            let writer_for_callback = Arc::clone(&writer);
            let _ = context
                .introspect()
                .get_sink_info_list(move |result| match result {
                    ListResult::Item(info) => {
                        if let Some(name) = info.name.as_deref() {
                            flags_for_callback
                                .outputs
                                .lock()
                                .expect("audio outputs lock poisoned")
                                .push(AudioDevice {
                                    name: name.to_owned(),
                                    display_name: info
                                        .description
                                        .as_deref()
                                        .unwrap_or(name)
                                        .to_owned(),
                                });
                        }
                    }
                    ListResult::End | ListResult::Error => {
                        flags_for_callback
                            .inventory_sinks_done
                            .store(true, Ordering::Release);
                        if !flags_for_callback
                            .inventory_sources_done
                            .load(Ordering::Acquire)
                        {
                            return;
                        }
                        let outputs = flags_for_callback
                            .outputs
                            .lock()
                            .expect("audio outputs lock poisoned")
                            .clone();
                        let inputs = flags_for_callback
                            .inputs
                            .lock()
                            .expect("audio inputs lock poisoned")
                            .clone();
                        publish(
                            &events_for_callback,
                            &writer_for_callback,
                            Event::AudioInventoryReceived { outputs, inputs },
                        );
                    }
                });
        }
        if flags.inventory_sources.swap(false, Ordering::AcqRel) {
            flags.inventory_sources_done.store(false, Ordering::Release);
            flags
                .inputs
                .lock()
                .expect("audio inputs lock poisoned")
                .clear();
            let flags_for_callback = Arc::clone(&flags);
            let events_for_callback = Arc::clone(&events);
            let writer_for_callback = Arc::clone(&writer);
            let _ = context
                .introspect()
                .get_source_info_list(move |result| match result {
                    ListResult::Item(info) => {
                        if info.monitor_of_sink.is_none() {
                            if let Some(name) = info.name.as_deref() {
                                flags_for_callback
                                    .inputs
                                    .lock()
                                    .expect("audio inputs lock poisoned")
                                    .push(AudioDevice {
                                        name: name.to_owned(),
                                        display_name: info
                                            .description
                                            .as_deref()
                                            .unwrap_or(name)
                                            .to_owned(),
                                    });
                            }
                        }
                    }
                    ListResult::End | ListResult::Error => {
                        flags_for_callback
                            .inventory_sources_done
                            .store(true, Ordering::Release);
                        if !flags_for_callback
                            .inventory_sinks_done
                            .load(Ordering::Acquire)
                        {
                            return;
                        }
                        let outputs = flags_for_callback
                            .outputs
                            .lock()
                            .expect("audio outputs lock poisoned")
                            .clone();
                        let inputs = flags_for_callback
                            .inputs
                            .lock()
                            .expect("audio inputs lock poisoned")
                            .clone();
                        publish(
                            &events_for_callback,
                            &writer_for_callback,
                            Event::AudioInventoryReceived { outputs, inputs },
                        );
                    }
                });
        }
        if flags.server.swap(false, Ordering::AcqRel) {
            let flags_for_callback = Arc::clone(&flags);
            let _ = context.introspect().get_server_info(move |info| {
                *flags_for_callback
                    .default_sink
                    .lock()
                    .expect("audio sink lock poisoned") =
                    info.default_sink_name.as_deref().map(str::to_owned);
                *flags_for_callback
                    .default_source
                    .lock()
                    .expect("audio source lock poisoned") = info
                    .default_source_name
                    .as_deref()
                    .filter(|name| !name.ends_with(".monitor"))
                    .map(str::to_owned);
                flags_for_callback.sink.store(true, Ordering::Release);
                flags_for_callback.source.store(true, Ordering::Release);
                flags_for_callback
                    .inventory_sinks
                    .store(true, Ordering::Release);
                flags_for_callback
                    .inventory_sources
                    .store(true, Ordering::Release);
            });
        }
        if flags.sink.swap(false, Ordering::AcqRel) {
            let Some(name) = flags
                .default_sink
                .lock()
                .expect("audio sink lock poisoned")
                .clone()
            else {
                publish(&events, &writer, Event::AudioUnavailable);
                continue;
            };
            let events_for_callback = Arc::clone(&events);
            let writer_for_callback = Arc::clone(&writer);
            let flags_for_callback = Arc::clone(&flags);
            let _ = context
                .introspect()
                .get_sink_info_by_name(&name, move |result| {
                    if let ListResult::Item(info) = result {
                        *flags_for_callback
                            .channels
                            .lock()
                            .expect("audio channels lock poisoned") = info.volume.len();
                        flags_for_callback.muted.store(info.mute, Ordering::Release);
                        let volume = info.volume.avg().0;
                        let percent = ((volume as u64 * 100 + 32_768) / 65_536) as u32;
                        flags_for_callback.volume.store(percent, Ordering::Release);
                        *flags_for_callback
                            .output_description
                            .lock()
                            .expect("audio description lock poisoned") =
                            info.description.as_deref().map(str::to_owned);
                        publish(
                            &events_for_callback,
                            &writer_for_callback,
                            Event::AudioSnapshotReceived(AudioState {
                                available: true,
                                default_output: info.name.as_deref().map(str::to_owned),
                                volume_percent: percent,
                                muted: info.mute,
                                default_input: flags_for_callback
                                    .default_source
                                    .lock()
                                    .expect("audio source lock poisoned")
                                    .clone(),
                                outputs: flags_for_callback
                                    .outputs
                                    .lock()
                                    .expect("audio outputs lock poisoned")
                                    .clone(),
                                inputs: flags_for_callback
                                    .inputs
                                    .lock()
                                    .expect("audio inputs lock poisoned")
                                    .clone(),
                                input_description: flags_for_callback
                                    .input_description
                                    .lock()
                                    .expect("audio input description lock poisoned")
                                    .clone(),
                                input_volume_percent: flags_for_callback
                                    .source_volume
                                    .load(Ordering::Acquire),
                                input_muted: flags_for_callback
                                    .source_muted
                                    .load(Ordering::Acquire),
                                output_description: flags_for_callback
                                    .output_description
                                    .lock()
                                    .expect("audio description lock poisoned")
                                    .clone(),
                            }),
                        );
                    }
                });
        }
        if flags.source.swap(false, Ordering::AcqRel) {
            if let Some(name) = flags
                .default_source
                .lock()
                .expect("audio source lock poisoned")
                .clone()
            {
                let events_for_callback = Arc::clone(&events);
                let writer_for_callback = Arc::clone(&writer);
                let flags_for_callback = Arc::clone(&flags);
                let _ = context
                    .introspect()
                    .get_source_info_by_name(&name, move |result| {
                        if let ListResult::Item(info) = result {
                            *flags_for_callback
                                .source_channels
                                .lock()
                                .expect("audio source channels lock poisoned") = info.volume.len();
                            flags_for_callback
                                .source_muted
                                .store(info.mute, Ordering::Release);
                            let volume = info.volume.avg().0;
                            let percent = ((volume as u64 * 100 + 32_768) / 65_536) as u32;
                            flags_for_callback
                                .source_volume
                                .store(percent, Ordering::Release);
                            *flags_for_callback
                                .input_description
                                .lock()
                                .expect("audio input description lock poisoned") =
                                info.description.as_deref().map(str::to_owned);
                            publish(
                                &events_for_callback,
                                &writer_for_callback,
                                Event::AudioSnapshotReceived(AudioState {
                                    available: true,
                                    default_output: flags_for_callback
                                        .default_sink
                                        .lock()
                                        .expect("audio sink lock poisoned")
                                        .clone(),
                                    outputs: flags_for_callback
                                        .outputs
                                        .lock()
                                        .expect("audio outputs lock poisoned")
                                        .clone(),
                                    inputs: flags_for_callback
                                        .inputs
                                        .lock()
                                        .expect("audio inputs lock poisoned")
                                        .clone(),
                                    volume_percent: flags_for_callback
                                        .volume
                                        .load(Ordering::Acquire),
                                    muted: flags_for_callback.muted.load(Ordering::Acquire),
                                    default_input: info.name.as_deref().map(str::to_owned),
                                    input_description: flags_for_callback
                                        .input_description
                                        .lock()
                                        .expect("audio input description lock poisoned")
                                        .clone(),
                                    input_volume_percent: percent,
                                    input_muted: info.mute,
                                    output_description: flags_for_callback
                                        .output_description
                                        .lock()
                                        .expect("audio description lock poisoned")
                                        .clone(),
                                }),
                            );
                        }
                    });
            }
        }
        for command in pending_commands
            .lock()
            .expect("audio command queue poisoned")
            .drain(..)
        {
            let mut introspector = context.introspect();
            match command {
                AudioCommand::SetOutputVolume(percent) => {
                    let Some(name) = flags
                        .default_sink
                        .lock()
                        .expect("audio sink lock poisoned")
                        .clone()
                    else {
                        continue;
                    };
                    let channels =
                        (*flags.channels.lock().expect("audio channels lock poisoned")).max(1);
                    let value = ((percent.min(100) as u64 * 65_536) / 100) as u32;
                    let mut volume = pulse::volume::ChannelVolumes::default();
                    volume.set(channels, pulse::volume::Volume(value));
                    let _ = introspector.set_sink_volume_by_name(&name, &volume, None);
                }
                AudioCommand::ToggleOutputMute => {
                    let Some(name) = flags
                        .default_sink
                        .lock()
                        .expect("audio sink lock poisoned")
                        .clone()
                    else {
                        continue;
                    };
                    let muted = !flags.muted.load(Ordering::Acquire);
                    let _ = introspector.set_sink_mute_by_name(&name, muted, None);
                }
                AudioCommand::SetInputVolume(percent) => {
                    let Some(input) = flags
                        .default_source
                        .lock()
                        .expect("audio source lock poisoned")
                        .clone()
                    else {
                        continue;
                    };
                    let channels = (*flags
                        .source_channels
                        .lock()
                        .expect("audio source channels lock poisoned"))
                    .max(1);
                    let value = ((percent.min(100) as u64 * 65_536) / 100) as u32;
                    let mut volume = pulse::volume::ChannelVolumes::default();
                    volume.set(channels, pulse::volume::Volume(value));
                    let _ = introspector.set_source_volume_by_name(&input, &volume, None);
                }
                AudioCommand::ToggleInputMute => {
                    let Some(input) = flags
                        .default_source
                        .lock()
                        .expect("audio source lock poisoned")
                        .clone()
                    else {
                        continue;
                    };
                    let muted = !flags.source_muted.load(Ordering::Acquire);
                    let _ = introspector.set_source_mute_by_name(&input, muted, None);
                }
                AudioCommand::SetDefaultOutput(name) => {
                    let _ = context.set_default_sink(&name, |_| {});
                }
                AudioCommand::SetDefaultInput(name) => {
                    let _ = context.set_default_source(&name, |_| {});
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{refresh_detail_for, Operation};

    #[test]
    fn removed_audio_object_never_requests_detail_refresh() {
        assert!(!refresh_detail_for(Some(Operation::Removed)));
        assert!(refresh_detail_for(Some(Operation::New)));
        assert!(refresh_detail_for(Some(Operation::Changed)));
        assert!(refresh_detail_for(None));
    }
}
