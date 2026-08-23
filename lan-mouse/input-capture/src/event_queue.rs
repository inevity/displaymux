use crate::{CaptureEvent, Position};
use futures::task::AtomicWaker;
use input_event::{Event, PointerEvent};
use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
    task::{Context, Poll},
};

// One down/up pair for every base and extended 8-bit scan-code slot, plus
// discrete pointer buttons/axes. Overflow still fails closed, so safety does
// not depend on this capacity; it only avoids teardown for a complete burst.
const SCAN_CODE_SLOTS: usize = 2 * (u8::MAX as usize + 1);
const POINTER_TRANSITIONS: usize = 16;
const CRITICAL_EVENT_CAPACITY: usize = 2 * SCAN_CODE_SLOTS + POINTER_TRANSITIONS;

pub(crate) struct EventQueue {
    inner: Mutex<QueueState>,
    waker: AtomicWaker,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<(Position, CaptureEvent)>,
    critical_count: usize,
    overflowed: bool,
    closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PushOutcome {
    Queued,
    Overflow,
}

pub(crate) enum QueuePoll {
    Event((Position, CaptureEvent)),
    Overflow,
    Closed,
}

impl EventQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(QueueState {
                events: VecDeque::with_capacity(CRITICAL_EVENT_CAPACITY),
                ..Default::default()
            }),
            waker: AtomicWaker::new(),
        }
    }

    pub(crate) fn push(&self, position: Position, event: CaptureEvent) -> PushOutcome {
        let mut state = self.lock();
        if state.closed || state.overflowed {
            return PushOutcome::Overflow;
        }
        if is_motion(&event) {
            if state
                .events
                .back()
                .is_some_and(|(_, queued)| is_motion(queued))
            {
                *state.events.back_mut().expect("motion event") = (position, event);
            } else {
                state.events.push_back((position, event));
            }
        } else if state.critical_count < CRITICAL_EVENT_CAPACITY {
            state.events.push_back((position, event));
            state.critical_count += 1;
        } else {
            state.events.clear();
            state.critical_count = 0;
            state.overflowed = true;
            drop(state);
            self.waker.wake();
            return PushOutcome::Overflow;
        }
        drop(state);
        self.waker.wake();
        PushOutcome::Queued
    }

    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<QueuePoll> {
        self.waker.register(cx.waker());
        let mut state = self.lock();
        if state.overflowed {
            state.overflowed = false;
            return Poll::Ready(QueuePoll::Overflow);
        }
        if let Some(event) = state.events.pop_front() {
            if !is_motion(&event.1) {
                state.critical_count -= 1;
            }
            return Poll::Ready(QueuePoll::Event(event));
        }
        if state.closed {
            return Poll::Ready(QueuePoll::Closed);
        }
        Poll::Pending
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.waker.wake();
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn is_motion(event: &CaptureEvent) -> bool {
    matches!(
        event,
        CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { .. }))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::{BTN_LEFT, PointerEvent};
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn context() -> Context<'static> {
        let waker = Waker::from(Arc::new(NoopWake));
        Context::from_waker(Box::leak(Box::new(waker)))
    }

    fn motion(dx: f64) -> CaptureEvent {
        CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx,
            dy: 0.0,
        }))
    }

    fn button(state: u32) -> CaptureEvent {
        CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state,
        }))
    }

    #[test]
    fn motion_is_coalesced_to_latest_sample() {
        let queue = EventQueue::new();
        assert_eq!(queue.push(Position::Left, motion(1.0)), PushOutcome::Queued);
        assert_eq!(queue.push(Position::Left, motion(2.0)), PushOutcome::Queued);

        let Poll::Ready(QueuePoll::Event((
            _,
            CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx, .. })),
        ))) = queue.poll(&mut context())
        else {
            panic!("latest motion was not available");
        };
        assert_eq!(dx, 2.0);
        assert!(queue.poll(&mut context()).is_pending());
    }

    #[test]
    fn motion_runs_are_coalesced_without_crossing_critical_events() {
        let queue = EventQueue::new();
        queue.push(Position::Left, motion(1.0));
        queue.push(Position::Left, motion(2.0));
        queue.push(Position::Left, button(1));
        queue.push(Position::Left, motion(3.0));
        queue.push(Position::Left, motion(4.0));
        queue.push(Position::Left, button(0));

        let mut observed = Vec::new();
        while let Poll::Ready(QueuePoll::Event((_, event))) = queue.poll(&mut context()) {
            observed.push(event);
        }

        assert!(matches!(
            &observed[..],
            [
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx: 2.0, .. })),
                CaptureEvent::Input(Event::Pointer(PointerEvent::Button { state: 1, .. })),
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx: 4.0, .. })),
                CaptureEvent::Input(Event::Pointer(PointerEvent::Button { state: 0, .. })),
            ]
        ));
    }

    #[test]
    fn critical_overflow_discards_buffer_and_surfaces_failure() {
        let queue = EventQueue::new();
        for state in 0..CRITICAL_EVENT_CAPACITY {
            assert_eq!(
                queue.push(Position::Left, button((state & 1) as u32)),
                PushOutcome::Queued
            );
        }
        assert_eq!(queue.push(Position::Left, button(1)), PushOutcome::Overflow);
        assert!(matches!(
            queue.poll(&mut context()),
            Poll::Ready(QueuePoll::Overflow)
        ));
        assert!(queue.poll(&mut context()).is_pending());
    }

    #[test]
    fn close_wakes_consumer_after_events_are_drained() {
        let queue = EventQueue::new();
        queue.push(Position::Left, button(1));
        queue.close();

        assert!(matches!(
            queue.poll(&mut context()),
            Poll::Ready(QueuePoll::Event(_))
        ));
        assert!(matches!(
            queue.poll(&mut context()),
            Poll::Ready(QueuePoll::Closed)
        ));
    }
}
