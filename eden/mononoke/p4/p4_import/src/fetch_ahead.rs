/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroUsize;
use std::task::Poll;

use futures::StreamExt;
use futures::channel::mpsc;
use futures::future;
use futures::stream::FuturesOrdered;

/// What `send_in_order` waits for.
enum Next<T> {
    Fetched(T),
    RoomToSend,
    /// The receiver has taken the output that was in the channel.
    Taken,
    ReceiverGone,
}

/// Runs `fetches` and sends their outputs to `sender`, a channel that holds one output, in
/// the order of `fetches`. At most `window` of them are running, waiting to be sent or in
/// the channel: the bound and the order of a `buffered(window)` stream sent to the channel
/// output by output.
///
/// Unlike that stream's, the fetches running are polled whenever they can make progress,
/// also while an output waits for the receiver. The stream polls its futures only when it
/// is polled itself, which is not while a send waits, so a receiver busy for longer than a
/// fetch's own timeout fails every fetch that still had something to read.
///
/// Returns once every output is sent, or once it finds the receiver gone, which drops the
/// fetches still running. It finds out when the receiver's going wakes it, which is with an
/// output in the channel, or when a fetch next makes progress: a caller that cannot wait
/// for that drops this future.
pub(crate) async fn send_in_order<F: Future>(
    fetches: impl IntoIterator<Item = F>,
    window: NonZeroUsize,
    mut sender: mpsc::Sender<F::Output>,
) {
    let mut fetches = fetches.into_iter();
    // Whether `fetches` has run out.
    let mut all_started = false;
    let mut running = FuturesOrdered::new();
    let mut fetched = VecDeque::new();
    let mut in_channel = false;
    loop {
        while !all_started && running.len() + fetched.len() + usize::from(in_channel) < window.get()
        {
            match fetches.next() {
                Some(fetch) => running.push_back(fetch),
                None => all_started = true,
            }
        }
        if all_started && running.is_empty() && fetched.is_empty() {
            return;
        }
        let next = future::poll_fn(|cx| {
            match sender.poll_ready(cx) {
                Poll::Ready(Err(_)) => return Poll::Ready(Next::ReceiverGone),
                Poll::Ready(Ok(())) if !fetched.is_empty() => {
                    return Poll::Ready(Next::RoomToSend);
                }
                Poll::Ready(Ok(())) if in_channel => return Poll::Ready(Next::Taken),
                Poll::Ready(Ok(())) | Poll::Pending => {}
            }
            match running.poll_next_unpin(cx) {
                Poll::Ready(Some(output)) => Poll::Ready(Next::Fetched(output)),
                // With nothing running, the receiver taking an output, or going, is what
                // wakes this: `poll_ready` above saw to it.
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            }
        })
        .await;
        match next {
            Next::Fetched(output) => fetched.push_back(output),
            Next::RoomToSend => {
                let Some(output) = fetched.pop_front() else {
                    continue;
                };
                match sender.try_send(output) {
                    Ok(()) => in_channel = true,
                    Err(error) if error.is_disconnected() => return,
                    // No room after all: the output keeps its place.
                    Err(error) => fetched.push_front(error.into_inner()),
                }
            }
            Next::Taken => in_channel = false,
            Next::ReceiverGone => return,
        }
    }
}
