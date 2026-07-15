use super::ClipboardBackend;
use crate::{ClipboardData, ClipboardReason, NativeGeneration};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    io::Read,
    os::fd::AsFd,
    time::{Duration, Instant},
};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
};
use wl_clipboard_rs::{
    copy::{MimeType as CopyMimeType, Options as CopyOptions, Source},
    paste::{
        ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat, get_contents,
        get_mime_types,
    },
};

const PRIVATE_MIME_TYPES: &[&str] = &[
    "application/x-kde-passwordManagerHint",
    "x-kde-passwordManagerHint",
];
const CAPTURE_DEADLINE: Duration = Duration::from_secs(4);
const READ_CHUNK_BYTES: usize = 64 * 1024;

enum DataControlManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Default)]
struct GenerationState {
    generation: u64,
    exhausted: bool,
}

impl GenerationState {
    fn selection_changed(&mut self) {
        match self.generation.checked_add(1) {
            Some(generation) => self.generation = generation,
            None => self.exhausted = true,
        }
    }
}

pub struct WaylandClipboardBackend {
    queue: EventQueue<GenerationState>,
    state: GenerationState,
    _manager: DataControlManager,
    _seats: Vec<WlSeat>,
    _ext_devices: Vec<ExtDataControlDeviceV1>,
    _wlr_devices: Vec<ZwlrDataControlDeviceV1>,
}

impl WaylandClipboardBackend {
    pub fn connect() -> Result<Self, ClipboardReason> {
        let connection =
            Connection::connect_to_env().map_err(|_| ClipboardReason::BackendUnavailable)?;
        let (globals, mut queue) = registry_queue_init::<GenerationState>(&connection)
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let qh = queue.handle();
        let manager = globals
            .bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ())
            .map(DataControlManager::Ext)
            .or_else(|_| {
                globals
                    .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
                    .map(DataControlManager::Wlr)
            })
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let registry = globals.registry();
        let seats = globals.contents().with_list(|listed| {
            listed
                .iter()
                .filter(|global| {
                    global.interface == WlSeat::interface().name && global.version >= 1
                })
                .map(|global| registry.bind(global.name, global.version.min(2), &qh, ()))
                .collect::<Vec<_>>()
        });
        if seats.is_empty() {
            return Err(ClipboardReason::BackendUnavailable);
        }
        let mut ext_devices = Vec::new();
        let mut wlr_devices = Vec::new();
        for seat in &seats {
            match &manager {
                DataControlManager::Ext(manager) => {
                    ext_devices.push(manager.get_data_device(seat, &qh, ()))
                }
                DataControlManager::Wlr(manager) => {
                    wlr_devices.push(manager.get_data_device(seat, &qh, ()))
                }
            }
        }
        let mut state = GenerationState::default();
        queue
            .roundtrip(&mut state)
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        Ok(Self {
            queue,
            state,
            _manager: manager,
            _seats: seats,
            _ext_devices: ext_devices,
            _wlr_devices: wlr_devices,
        })
    }

    fn synchronized_generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        if self.state.exhausted {
            return Err(ClipboardReason::IdentityExhausted);
        }
        Ok(NativeGeneration::new(self.state.generation))
    }

    fn capture_text(max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        let mime_types = match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(mime_types) => mime_types,
            Err(PasteError::ClipboardEmpty) => return Ok(ClipboardData::Empty),
            Err(error) => return Err(map_paste_error(error)),
        };
        if mime_types
            .iter()
            .any(|mime| PRIVATE_MIME_TYPES.iter().any(|private| mime == private))
        {
            return Err(ClipboardReason::PrivateContent);
        }
        let (pipe, _) = get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            PasteMimeType::Text,
        )
        .map_err(map_paste_error)?;
        let bytes = read_bounded_pipe(pipe, max_bytes, CAPTURE_DEADLINE)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| ClipboardReason::InvalidUtf8)?;
        let normalized = normalize_line_endings(text);
        if normalized.is_empty() {
            Ok(ClipboardData::Empty)
        } else {
            ClipboardData::text(normalized.into_bytes())
        }
    }

    fn apply_data(data: &ClipboardData) -> Result<(), ClipboardReason> {
        let bytes: Box<[u8]> = match data {
            ClipboardData::Text(bytes) => bytes.as_ref().into(),
            ClipboardData::Empty => Box::default(),
            ClipboardData::Unavailable(reason) => return Err(*reason),
        };
        CopyOptions::new()
            .copy(Source::Bytes(bytes), CopyMimeType::Text)
            .map_err(|_| ClipboardReason::BackendUnavailable)
    }
}

fn read_bounded_pipe(
    mut pipe: impl Read + AsFd,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, ClipboardReason> {
    let limit = max_bytes.checked_add(1).ok_or(ClipboardReason::Oversize)?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::with_capacity(max_bytes.min(READ_CHUNK_BYTES));
    let mut chunk = vec![0; limit.min(READ_CHUNK_BYTES)];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ClipboardReason::TransferTimeout)?;
        let timeout = Timespec {
            tv_sec: i64::try_from(remaining.as_secs())
                .map_err(|_| ClipboardReason::TransferTimeout)?,
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut poll_fd = [PollFd::new(&pipe, PollFlags::IN)];
        if poll(&mut poll_fd, Some(&timeout)).map_err(|_| ClipboardReason::BackendUnavailable)? == 0
        {
            return Err(ClipboardReason::TransferTimeout);
        }
        let ready = poll_fd[0].revents();
        if ready.intersects(PollFlags::ERR | PollFlags::NVAL) {
            return Err(ClipboardReason::BackendUnavailable);
        }
        let remaining_limit = limit.saturating_sub(bytes.len());
        let read_limit = remaining_limit.min(chunk.len());
        let read = pipe
            .read(&mut chunk[..read_limit])
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        if read == 0 {
            return if bytes.len() > max_bytes {
                Err(ClipboardReason::Oversize)
            } else {
                Ok(bytes)
            };
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > max_bytes {
            return Err(ClipboardReason::Oversize);
        }
    }
}

impl ClipboardBackend for WaylandClipboardBackend {
    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.synchronized_generation()
    }

    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        Self::capture_text(max_bytes)
    }

    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason> {
        if self.synchronized_generation()? != expected_generation {
            return Err(ClipboardReason::DestinationChanged);
        }
        Self::apply_data(data)?;
        self.synchronized_generation()
    }
}

fn map_paste_error(error: PasteError) -> ClipboardReason {
    match error {
        PasteError::ClipboardEmpty => ClipboardReason::BackendUnavailable,
        PasteError::NoMimeType => ClipboardReason::UnsupportedFormat,
        _ => ClipboardReason::BackendUnavailable,
    }
}

fn normalize_line_endings(text: &str) -> String {
    if !text.as_bytes().contains(&b'\r') {
        return text.to_owned();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl Dispatch<WlRegistry, GlobalListContents> for GenerationState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for GenerationState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! empty_dispatch {
    ($interface:ty) => {
        impl Dispatch<$interface, ()> for GenerationState {
            fn event(
                _state: &mut Self,
                _proxy: &$interface,
                _event: <$interface as Proxy>::Event,
                _data: &(),
                _connection: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

empty_dispatch!(ExtDataControlManagerV1);
empty_dispatch!(ZwlrDataControlManagerV1);
empty_dispatch!(ExtDataControlOfferV1);
empty_dispatch!(ZwlrDataControlOfferV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for GenerationState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, ext_data_control_device_v1::Event::Selection { .. }) {
            state.selection_changed();
        }
    }

    event_created_child!(GenerationState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for GenerationState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, zwlr_data_control_device_v1::Event::Selection { .. }) {
            state.selection_changed();
        }
    }

    event_created_child!(GenerationState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::net::UnixStream, thread};

    #[test]
    fn line_endings_are_canonical_utf8_text() {
        assert_eq!(normalize_line_endings("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn private_markers_are_explicit_and_content_independent() {
        assert!(PRIVATE_MIME_TYPES.contains(&"x-kde-passwordManagerHint"));
        assert!(
            !PRIVATE_MIME_TYPES
                .iter()
                .any(|mime| mime.contains("secret text"))
        );
    }

    #[test]
    fn bounded_pipe_returns_at_eof_and_accepts_exact_limit() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || writer.write_all(b"text").unwrap());
        assert_eq!(
            read_bounded_pipe(reader, 4, Duration::from_secs(1)).unwrap(),
            b"text"
        );
        sender.join().unwrap();
    }

    #[test]
    fn bounded_pipe_rejects_one_byte_over_limit_during_accumulation() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || writer.write_all(b"texts").unwrap());
        assert_eq!(
            read_bounded_pipe(reader, 4, Duration::from_secs(1)),
            Err(ClipboardReason::Oversize)
        );
        sender.join().unwrap();
    }

    #[test]
    fn bounded_pipe_times_out_when_source_never_produces_or_closes() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        assert_eq!(
            read_bounded_pipe(reader, 4, Duration::from_millis(10)),
            Err(ClipboardReason::TransferTimeout)
        );
    }

    #[test]
    #[ignore = "requires the interactive user's Wayland session"]
    fn live_generation_monitor_connects_without_reading_clipboard_data() {
        let mut backend = WaylandClipboardBackend::connect().expect("Wayland data-control backend");
        backend.generation().expect("synchronized generation");
    }
}
