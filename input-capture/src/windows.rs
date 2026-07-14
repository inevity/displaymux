use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::{pin::Pin, sync::Arc};

use super::{
    Capture, CaptureError, CaptureEvent, Position,
    event_queue::{EventQueue, QueuePoll},
};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_queue: Arc<EventQueue>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.create(pos);
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.destroy(pos);
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture().await;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        let event_queue = Arc::new(EventQueue::new());
        let event_thread = EventThread::new(event_queue.clone());
        Self {
            event_thread,
            event_queue,
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.event_queue.poll(cx) {
            Poll::Ready(QueuePoll::Event(event)) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(QueuePoll::Overflow) => {
                Poll::Ready(Some(Err(CaptureError::CriticalQueueOverflow)))
            }
            Poll::Ready(QueuePoll::Closed) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
