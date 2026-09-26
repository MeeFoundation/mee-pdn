//! The fan-out of an actor's events to its subscribers. The actor waits on a
//! [`Delivery::Blocking`] subscriber only, and only code of this crate
//! subscribes that way: a consumer outside it that stops reading would
//! otherwise stop the actor, and every replica the actor serves with it.

use n0_future::IterExt;

/// An event that stands for events dropped before it.
pub(crate) trait LagNotice: Clone {
    /// The notice replacing `self` and whatever else the buffer could not take.
    fn lagged(&self) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The actor waits for room in the channel: no event is lost.
    Blocking,
    /// A full channel drops the event, and a lag notice follows the last one
    /// the subscriber still receives.
    Lossy,
}

#[derive(Debug)]
struct Subscriber<T> {
    tx: async_channel::Sender<T>,
    delivery: Delivery,
    /// Events this side put into the channel; with its length, how many the
    /// subscriber took. Exact while the actor is the channel's only producer.
    sent: u64,
    /// Sequence number of the last lag notice put into the channel.
    notice: Option<u64>,
}

impl<T: LagNotice> Subscriber<T> {
    /// `false` once the subscriber is gone.
    fn offer(&mut self, event: T) -> bool {
        let Some(capacity) = self.tx.capacity() else {
            return self.put(event);
        };
        let len = self.tx.len();
        // The last slot is kept for the notice, so one always fits.
        if len.saturating_add(1) < capacity {
            return self.put(event);
        }
        let taken = self
            .sent
            .saturating_sub(u64::try_from(len).unwrap_or(u64::MAX));
        // A notice still unread is read after this event was emitted, so it
        // stands for this one too.
        if self.notice.is_some_and(|notice| taken <= notice) {
            return !self.tx.is_closed();
        }
        let notice = event.lagged();
        let seq = self.sent;
        let alive = self.put(notice);
        if alive {
            self.notice = Some(seq);
        }
        alive
    }

    fn put(&mut self, event: T) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => {
                self.sent = self.sent.saturating_add(1);
                true
            }
            Err(async_channel::TrySendError::Full(_)) => true,
            Err(async_channel::TrySendError::Closed(_)) => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Subscribers<T>(Vec<Subscriber<T>>);

impl<T> Default for Subscribers<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T: LagNotice> Subscribers<T> {
    pub(crate) fn subscribe(&mut self, tx: async_channel::Sender<T>, delivery: Delivery) {
        self.0.push(Subscriber {
            tx,
            delivery,
            sent: 0,
            notice: None,
        });
    }

    pub(crate) fn unsubscribe(&mut self, tx: &async_channel::Sender<T>) {
        self.0.retain(|s| !s.tx.same_channel(tx));
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drops every subscriber whose receiver is gone.
    pub(crate) async fn send(&mut self, event: T) {
        let mut kept = Vec::with_capacity(self.0.len());
        let mut blocking = Vec::new();
        for mut subscriber in std::mem::take(&mut self.0) {
            match subscriber.delivery {
                Delivery::Lossy => {
                    if subscriber.offer(event.clone()) {
                        kept.push(subscriber);
                    }
                }
                Delivery::Blocking => blocking.push(subscriber),
            }
        }
        let delivered = blocking
            .into_iter()
            .map(async |subscriber| {
                subscriber
                    .tx
                    .send(event.clone())
                    .await
                    .ok()
                    .map(|()| subscriber)
            })
            .join_all()
            .await;
        kept.extend(delivered.into_iter().flatten());
        self.0 = kept;
    }

    pub(crate) async fn send_with(&mut self, f: impl FnOnce() -> T) {
        if !self.is_empty() {
            self.send(f()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ev {
        N(u32),
        Lag,
    }

    impl LagNotice for Ev {
        fn lagged(&self) -> Self {
            Self::Lag
        }
    }

    fn drain(rx: &async_channel::Receiver<Ev>) -> Vec<Ev> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A lossy subscriber that stops reading holds up no send, and one lag
    /// notice follows the last event it still receives.
    #[tokio::test]
    async fn a_lossy_subscriber_that_stops_reading_holds_up_no_send() {
        let (tx, rx) = async_channel::bounded(4);
        let mut subscribers = Subscribers::default();
        subscribers.subscribe(tx, Delivery::Lossy);
        for n in 0..10 {
            tokio::time::timeout(Duration::from_secs(1), subscribers.send(Ev::N(n)))
                .await
                .expect("a lossy send waits on no reader");
        }
        assert_eq!(
            drain(&rx),
            vec![Ev::N(0), Ev::N(1), Ev::N(2), Ev::Lag],
            "the events past the buffer are dropped behind one notice"
        );
        subscribers.send(Ev::N(10)).await;
        assert_eq!(
            drain(&rx),
            vec![Ev::N(10)],
            "a drained channel takes events again"
        );
    }

    /// Events dropped after a notice was read get a notice of their own; ones
    /// dropped while it is unread do not.
    #[tokio::test]
    async fn a_notice_already_read_is_followed_by_another() {
        let (tx, rx) = async_channel::bounded(3);
        let mut subscribers = Subscribers::default();
        subscribers.subscribe(tx, Delivery::Lossy);
        for n in 0..4 {
            subscribers.send(Ev::N(n)).await;
        }
        assert_eq!(rx.try_recv().ok(), Some(Ev::N(0)));
        // The freed slot is the one kept for a notice, and the notice is unread.
        subscribers.send(Ev::N(4)).await;
        assert_eq!(drain(&rx), vec![Ev::N(1), Ev::Lag]);
        for n in 6..10 {
            subscribers.send(Ev::N(n)).await;
        }
        assert_eq!(
            drain(&rx),
            vec![Ev::N(6), Ev::N(7), Ev::Lag],
            "a drop after the first notice was read gets a second one"
        );
    }

    /// A blocking subscriber receives every event, in order, the send waiting
    /// for room.
    #[tokio::test]
    async fn a_blocking_subscriber_loses_nothing() {
        let (tx, rx) = async_channel::bounded(2);
        let mut subscribers = Subscribers::default();
        subscribers.subscribe(tx, Delivery::Blocking);
        let reader = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Ok(ev) = rx.recv().await {
                got.push(ev);
            }
            got
        });
        for n in 0..10 {
            subscribers.send(Ev::N(n)).await;
        }
        drop(subscribers);
        let got = reader.await.expect("reader");
        assert_eq!(got, (0..10).map(Ev::N).collect::<Vec<_>>());
    }

    /// A subscriber whose receiver is gone is dropped, whatever its delivery.
    #[tokio::test]
    async fn a_subscriber_whose_receiver_is_gone_is_dropped() {
        let mut subscribers = Subscribers::default();
        let (lossy, lossy_rx) = async_channel::bounded(4);
        let (blocking, blocking_rx) = async_channel::bounded(4);
        subscribers.subscribe(lossy, Delivery::Lossy);
        subscribers.subscribe(blocking, Delivery::Blocking);
        drop(lossy_rx);
        drop(blocking_rx);
        subscribers.send(Ev::N(0)).await;
        assert!(subscribers.is_empty());
    }
}
