//! TEMP, for Omniphony Studio 0.6.0: what the render path tells clients about
//! audio its host has not played yet, held until the host has.
//!
//! The engine describes each block - the spatial frame and its objects, the
//! timestamp, the meters - as it renders it, and a host that buffers what it
//! is handed plays that block later: Kodi's binaural codec banks up to a second
//! and a half of rendered audio, and its audio engine and sink hold most of
//! another second behind that. Upstream's answer ([`super::playout`]) leaves
//! the waiting to the client, which Studio 0.6.0 does not do, so it draws the
//! objects that far ahead of the sound. Until a Studio release that follows
//! `/omniphony/playout/heard` is current, an embedded host that says where the
//! listener is (`heard_us`) gets the description when the listener reaches the
//! block instead: each message, its block's marker included, is kept with the
//! position of the block it describes and goes out in order once that
//! position has been heard. A Studio that follows the sound then finds every
//! block already heard and shows it at once.
//!
//! Drop this module, and the calls to it, once that Studio is released.

use std::collections::VecDeque;

/// Held messages beyond this go out early, oldest first, rather than being
/// kept without bound by a host that has stopped saying where the listener
/// is. Several seconds of the busiest stream: an object frame is a few hundred
/// bytes, at most one per decoded frame.
const MAX_HELD_BYTES: usize = 8 << 20;

/// Which clients a message was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Audience {
    All,
    Metering,
}

struct Held {
    at: u64,
    to: Audience,
    bytes: Vec<u8>,
}

pub(crate) struct Hold {
    /// The sample position the listener has reached.
    heard: u64,
    /// Where the block being rendered starts: what the messages sent now are
    /// about.
    at: u64,
    held: VecDeque<Held>,
    held_bytes: usize,
}

impl Hold {
    pub(crate) fn new(heard: u64) -> Self {
        Self {
            heard,
            at: 0,
            held: VecDeque::new(),
            held_bytes: 0,
        }
    }

    /// The listener has reached `pos`.
    pub(crate) fn heard(&mut self, pos: u64) {
        self.heard = pos;
    }

    /// The messages sent from now on describe the block starting at `pos`.
    pub(crate) fn render_at(&mut self, pos: u64) {
        self.at = pos;
    }

    /// The stream starts again from 0, as it does after a reset: what is held
    /// was about audio nobody will hear.
    pub(crate) fn rewind(&mut self) {
        self.heard = 0;
        self.at = 0;
        self.held.clear();
        self.held_bytes = 0;
    }

    /// Keep `bytes` if it describes a block the listener has not reached, or
    /// if anything is held ahead of it, which it must not overtake. False
    /// means it can go now.
    pub(crate) fn keep(&mut self, to: Audience, bytes: &[u8]) -> bool {
        if self.held.is_empty() && self.at <= self.heard {
            return false;
        }
        self.held_bytes += bytes.len();
        self.held.push_back(Held {
            at: self.at,
            to,
            bytes: bytes.to_vec(),
        });
        true
    }

    /// The oldest held message, once the listener has reached its block or
    /// more than [`MAX_HELD_BYTES`] is held.
    pub(crate) fn next_due(&mut self) -> Option<(Audience, Vec<u8>)> {
        let front = self.held.front()?;
        if front.at > self.heard && self.held_bytes <= MAX_HELD_BYTES {
            return None;
        }
        let held = self.held.pop_front()?;
        self.held_bytes -= held.bytes.len();
        Some((held.to, held.bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due(h: &mut Hold) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| h.next_due().map(|(_, bytes)| bytes)).collect()
    }

    #[test]
    fn a_block_is_described_once_the_listener_reaches_it() {
        let mut h = Hold::new(0);
        h.render_at(0);
        assert!(
            !h.keep(Audience::All, b"a"),
            "the block being heard goes now"
        );
        h.render_at(480);
        assert!(h.keep(Audience::All, b"b"));
        h.render_at(960);
        assert!(h.keep(Audience::Metering, b"c"));
        assert!(due(&mut h).is_empty(), "nothing heard past 0 yet");

        h.heard(479);
        assert!(due(&mut h).is_empty());
        h.heard(480);
        assert_eq!(due(&mut h), vec![b"b".to_vec()]);
        h.heard(2000);
        assert_eq!(h.next_due(), Some((Audience::Metering, b"c".to_vec())));
        assert_eq!(h.next_due(), None);
    }

    #[test]
    fn nothing_overtakes_what_is_held() {
        let mut h = Hold::new(0);
        h.render_at(480);
        assert!(h.keep(Audience::All, b"b"));
        // A message about audio already heard still waits its turn, or a
        // client would see the frames out of order.
        h.render_at(0);
        assert!(h.keep(Audience::All, b"late"));
        h.heard(480);
        assert_eq!(due(&mut h), vec![b"b".to_vec(), b"late".to_vec()]);
        assert!(!h.keep(Audience::All, b"now"), "nothing held any more");
    }

    #[test]
    fn a_rewind_forgets_what_nobody_will_hear() {
        let mut h = Hold::new(0);
        h.render_at(48_000);
        assert!(h.keep(Audience::All, b"old"));
        h.rewind();
        assert_eq!(h.next_due(), None);
        h.render_at(0);
        assert!(
            !h.keep(Audience::All, b"new"),
            "position 0 is where it starts"
        );
        h.render_at(480);
        assert!(h.keep(Audience::All, b"later"));
    }

    #[test]
    fn a_host_that_stops_reporting_cannot_hold_without_bound() {
        let mut h = Hold::new(0);
        h.render_at(1);
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..8 {
            assert!(h.keep(Audience::All, &chunk));
        }
        assert_eq!(h.next_due(), None, "at the limit, still held");
        assert!(h.keep(Audience::All, b"x"));
        assert_eq!(h.next_due().map(|(_, b)| b.len()), Some(1 << 20));
        assert_eq!(h.next_due(), None, "back under the limit");
    }
}
