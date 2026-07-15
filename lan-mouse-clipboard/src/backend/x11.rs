use super::ClipboardBackend;
use crate::{ClipboardData, ClipboardReason, NativeGeneration};
use arboard::{Clipboard, LinuxClipboardKind, SetExtLinux};
use std::{
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
    connection::Connection,
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property,
            SelectionNotifyEvent, Time, WindowClass,
        },
    },
    rust_connection::RustConnection,
};

// This is one fixed actor-operation budget, not a user-visible switch timeout. It bounds work by
// an untrusted X11 selection owner while still matching the mature four-second X11 clipboard wait.
const CAPTURE_DEADLINE: Duration = Duration::from_secs(4);
const POLL_INTERVAL: Duration = Duration::from_millis(1);
// TARGETS is metadata, not clipboard content. Refuse an implausibly large list instead of allowing
// an X11 owner to allocate unbounded memory before the configured payload limit can be enforced.
const MAX_TARGET_ATOMS: u32 = 4096;

x11rb::atom_manager! {
    Atoms: AtomCookies {
        CLIPBOARD,
        TARGETS,
        ATOM,
        INCR,
        UTF8_STRING,
        UTF8_MIME_LOWER: b"text/plain;charset=utf-8",
        UTF8_MIME_UPPER: b"text/plain;charset=UTF-8",
        TEXT,
        TEXT_MIME: b"text/plain",
        KDE_PASSWORD_HINT: b"application/x-kde-passwordManagerHint",
        KDE_PASSWORD_HINT_LEGACY: b"x-kde-passwordManagerHint",
        LAN_MOUSE_CLIPBOARD,
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedBytes {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
            max_bytes,
        }
    }

    fn push(&mut self, chunk: &[u8], bytes_after: u32) -> Result<(), ClipboardReason> {
        let new_len = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(ClipboardReason::Oversize)?;
        if new_len > self.max_bytes || bytes_after != 0 {
            return Err(ClipboardReason::Oversize);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

pub struct X11ClipboardBackend {
    connection: RustConnection,
    window: u32,
    atoms: Atoms,
    generation: u64,
    generation_exhausted: bool,
    owner: Clipboard,
}

impl X11ClipboardBackend {
    pub fn connect() -> Result<Self, ClipboardReason> {
        let (connection, screen_number) =
            RustConnection::connect(None).map_err(|_| ClipboardReason::BackendUnavailable)?;
        connection
            .xfixes_query_version(5, 0)
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let screen = connection
            .setup()
            .roots
            .get(screen_number)
            .ok_or(ClipboardReason::BackendUnavailable)?;
        let window = connection
            .generate_id()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::COPY_FROM_PARENT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let atoms = Atoms::new(&connection)
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        connection
            .xfixes_select_selection_input(
                window,
                atoms.CLIPBOARD,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        connection
            .flush()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        let owner = Clipboard::new().map_err(|_| ClipboardReason::BackendUnavailable)?;
        let mut backend = Self {
            connection,
            window,
            atoms,
            generation: 0,
            generation_exhausted: false,
            owner,
        };
        backend.synchronized_generation()?;
        Ok(backend)
    }

    fn synchronized_generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.connection
            .get_input_focus()
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        while let Some(event) = self
            .connection
            .poll_for_event()
            .map_err(|_| ClipboardReason::BackendUnavailable)?
        {
            self.observe_generation_event(&event);
        }
        if self.generation_exhausted {
            return Err(ClipboardReason::IdentityExhausted);
        }
        Ok(NativeGeneration::new(self.generation))
    }

    fn observe_generation_event(&mut self, event: &Event) {
        if matches!(event, Event::XfixesSelectionNotify(event) if event.selection == self.atoms.CLIPBOARD)
        {
            match self.generation.checked_add(1) {
                Some(generation) => self.generation = generation,
                None => self.generation_exhausted = true,
            }
        }
    }

    fn clipboard_owner(&self) -> Result<u32, ClipboardReason> {
        self.connection
            .get_selection_owner(self.atoms.CLIPBOARD)
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map(|reply| reply.owner)
            .map_err(|_| ClipboardReason::BackendUnavailable)
    }

    fn prepare_conversion(&self, target: Atom) -> Result<(), ClipboardReason> {
        self.connection
            .delete_property(self.window, self.atoms.LAN_MOUSE_CLIPBOARD)
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        self.connection
            .convert_selection(
                self.window,
                self.atoms.CLIPBOARD,
                target,
                self.atoms.LAN_MOUSE_CLIPBOARD,
                Time::CURRENT_TIME,
            )
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        self.connection
            .flush()
            .map_err(|_| ClipboardReason::BackendUnavailable)
    }

    fn wait_for_selection_notify(
        &mut self,
        target: Atom,
        deadline: Instant,
    ) -> Result<SelectionNotifyEvent, ClipboardReason> {
        while Instant::now() < deadline {
            match self
                .connection
                .poll_for_event()
                .map_err(|_| ClipboardReason::BackendUnavailable)?
            {
                Some(event) => {
                    self.observe_generation_event(&event);
                    if let Event::SelectionNotify(event) = event {
                        if event.requestor == self.window
                            && event.selection == self.atoms.CLIPBOARD
                            && event.target == target
                        {
                            if event.property == NONE {
                                return Err(ClipboardReason::UnsupportedFormat);
                            }
                            return Ok(event);
                        }
                    }
                }
                None => thread::sleep(POLL_INTERVAL),
            }
        }
        Err(ClipboardReason::TransferTimeout)
    }

    fn read_targets(&mut self, deadline: Instant) -> Result<Vec<Atom>, ClipboardReason> {
        self.prepare_conversion(self.atoms.TARGETS)?;
        let event = self.wait_for_selection_notify(self.atoms.TARGETS, deadline)?;
        let reply = self
            .connection
            .get_property(
                true,
                event.requestor,
                event.property,
                AtomEnum::ANY,
                0,
                MAX_TARGET_ATOMS,
            )
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        if reply.bytes_after != 0 {
            let _ = self
                .connection
                .delete_property(event.requestor, event.property);
            return Err(ClipboardReason::UnsupportedFormat);
        }
        if reply.type_ == self.atoms.INCR || reply.type_ != self.atoms.ATOM || reply.format != 32 {
            return Err(ClipboardReason::UnsupportedFormat);
        }
        reply
            .value32()
            .map(Iterator::collect)
            .ok_or(ClipboardReason::UnsupportedFormat)
    }

    fn read_text(
        &mut self,
        target: Atom,
        max_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, ClipboardReason> {
        self.prepare_conversion(target)?;
        let event = self.wait_for_selection_notify(target, deadline)?;
        let long_length = max_bytes
            .checked_add(1)
            .and_then(|bytes| bytes.checked_add(3))
            .and_then(|bytes| u32::try_from(bytes / 4).ok())
            .ok_or(ClipboardReason::Oversize)?;
        let reply = self
            .connection
            .get_property(
                true,
                event.requestor,
                event.property,
                AtomEnum::ANY,
                0,
                long_length,
            )
            .map_err(|_| ClipboardReason::BackendUnavailable)?
            .reply()
            .map_err(|_| ClipboardReason::BackendUnavailable)?;
        if reply.type_ == target {
            let mut bytes = BoundedBytes::new(max_bytes);
            bytes.push(&reply.value, reply.bytes_after)?;
            return Ok(bytes.finish());
        }
        if reply.type_ != self.atoms.INCR {
            return Err(ClipboardReason::UnsupportedFormat);
        }

        let mut bytes = BoundedBytes::new(max_bytes);
        while Instant::now() < deadline {
            match self
                .connection
                .poll_for_event()
                .map_err(|_| ClipboardReason::BackendUnavailable)?
            {
                Some(event) => {
                    self.observe_generation_event(&event);
                    let Event::PropertyNotify(event) = event else {
                        continue;
                    };
                    if event.window != self.window
                        || event.atom != self.atoms.LAN_MOUSE_CLIPBOARD
                        || event.state != Property::NEW_VALUE
                    {
                        continue;
                    }
                    let remaining = max_bytes.saturating_sub(bytes.bytes.len());
                    let long_length = remaining
                        .checked_add(1)
                        .and_then(|value| value.checked_add(3))
                        .and_then(|value| u32::try_from(value / 4).ok())
                        .ok_or(ClipboardReason::Oversize)?;
                    let reply = self
                        .connection
                        .get_property(true, event.window, event.atom, target, 0, long_length)
                        .map_err(|_| ClipboardReason::BackendUnavailable)?
                        .reply()
                        .map_err(|_| ClipboardReason::BackendUnavailable)?;
                    if reply.type_ != target {
                        return Err(ClipboardReason::UnsupportedFormat);
                    }
                    if reply.value.is_empty() && reply.bytes_after == 0 {
                        return Ok(bytes.finish());
                    }
                    if let Err(reason) = bytes.push(&reply.value, reply.bytes_after) {
                        let _ = self.connection.delete_property(event.window, event.atom);
                        return Err(reason);
                    }
                }
                None => thread::sleep(POLL_INTERVAL),
            }
        }
        Err(ClipboardReason::TransferTimeout)
    }

    fn capture_text(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        if self.clipboard_owner()? == NONE {
            return Ok(ClipboardData::Empty);
        }
        let deadline = Instant::now() + CAPTURE_DEADLINE;
        let targets = self.read_targets(deadline)?;
        if targets.contains(&self.atoms.KDE_PASSWORD_HINT)
            || targets.contains(&self.atoms.KDE_PASSWORD_HINT_LEGACY)
        {
            return Err(ClipboardReason::PrivateContent);
        }
        let target = [
            self.atoms.UTF8_STRING,
            self.atoms.UTF8_MIME_LOWER,
            self.atoms.UTF8_MIME_UPPER,
            self.atoms.TEXT_MIME,
            self.atoms.TEXT,
        ]
        .into_iter()
        .find(|target| targets.contains(target))
        .ok_or(ClipboardReason::UnsupportedFormat)?;
        let bytes = self.read_text(target, max_bytes, deadline)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| ClipboardReason::InvalidUtf8)?;
        let normalized = normalize_line_endings(text);
        if normalized.is_empty() {
            Ok(ClipboardData::Empty)
        } else {
            ClipboardData::text(normalized.into_bytes())
        }
    }

    fn apply_data(&mut self, data: &ClipboardData) -> Result<(), ClipboardReason> {
        let text = match data {
            ClipboardData::Text(bytes) => {
                std::str::from_utf8(bytes).map_err(|_| ClipboardReason::InvalidUtf8)?
            }
            ClipboardData::Empty => "",
            ClipboardData::Unavailable(reason) => return Err(*reason),
        };
        self.owner
            .set()
            .clipboard(LinuxClipboardKind::Clipboard)
            .text(text.to_owned())
            .map_err(|_| ClipboardReason::BackendUnavailable)
    }
}

impl ClipboardBackend for X11ClipboardBackend {
    fn generation(&mut self) -> Result<NativeGeneration, ClipboardReason> {
        self.synchronized_generation()
    }

    fn capture(&mut self, max_bytes: usize) -> Result<ClipboardData, ClipboardReason> {
        self.capture_text(max_bytes)
    }

    fn apply(
        &mut self,
        expected_generation: NativeGeneration,
        data: &ClipboardData,
    ) -> Result<NativeGeneration, ClipboardReason> {
        if self.synchronized_generation()? != expected_generation {
            return Err(ClipboardReason::DestinationChanged);
        }
        self.apply_data(data)?;
        self.synchronized_generation()
    }
}

fn normalize_line_endings(text: &str) -> String {
    if !text.as_bytes().contains(&b'\r') {
        return text.to_owned();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_direct_read_accepts_exact_limit() {
        let mut bytes = BoundedBytes::new(4);
        bytes.push(b"text", 0).unwrap();
        assert_eq!(bytes.finish(), b"text");
    }

    #[test]
    fn bounded_direct_read_rejects_one_byte_over_limit() {
        let mut bytes = BoundedBytes::new(4);
        assert_eq!(bytes.push(b"texts", 0), Err(ClipboardReason::Oversize));
    }

    #[test]
    fn bounded_incr_read_checks_each_chunk_before_accumulating() {
        let mut bytes = BoundedBytes::new(6);
        bytes.push(b"abc", 0).unwrap();
        bytes.push(b"def", 0).unwrap();
        assert_eq!(bytes.push(b"g", 0), Err(ClipboardReason::Oversize));
        assert_eq!(bytes.finish(), b"abcdef");
    }

    #[test]
    fn unread_property_tail_is_oversize() {
        let mut bytes = BoundedBytes::new(8);
        assert_eq!(bytes.push(b"text", 1), Err(ClipboardReason::Oversize));
    }

    #[test]
    fn line_endings_are_canonical_utf8_text() {
        assert_eq!(normalize_line_endings("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    #[ignore = "requires the interactive user's X11 session"]
    fn live_generation_monitor_connects_without_reading_clipboard_data() {
        let mut backend = X11ClipboardBackend::connect().expect("X11 clipboard backend");
        backend.generation().expect("synchronized generation");
    }
}
