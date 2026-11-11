//! "Last N minutes" buffer: already-compressed packets of all tracks kept in memory,
//! in arrival order. Cuts always happen at video keyframes.

use std::collections::VecDeque;

use crate::encoder::Pkt;

pub struct Ring {
    q: VecDeque<Pkt>,
    bytes: usize,
    /// How much history to keep, microseconds.
    max_span: i64,
    max_bytes: usize,
    /// Time of the newest video packet.
    last_us: i64,
}

impl Ring {
    pub fn new(max_span_us: i64, max_bytes: usize) -> Self {
        Self { q: VecDeque::new(), bytes: 0, max_span: max_span_us, max_bytes, last_us: 0 }
    }

    pub fn clear(&mut self) {
        self.q.clear();
        self.bytes = 0;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Time of the newest picture in the buffer, microseconds on the pipeline clock.
    pub fn last_us(&self) -> i64 {
        self.last_us
    }

    /// Length of the stored history in microseconds.
    pub fn span(&self) -> i64 {
        self.q.front().map_or(0, |first| self.last_us - first.us)
    }

    pub fn push(&mut self, pkt: Pkt) {
        // The buffer must always start with a video keyframe, otherwise it cannot be played.
        if self.q.is_empty() && !pkt.is_video_key() {
            return;
        }
        if pkt.stream == 0 {
            // With B-frames pictures arrive out of display order: keep the latest moment seen.
            self.last_us = self.last_us.max(pkt.us);
        }
        self.bytes += pkt.data.len();
        self.q.push_back(pkt);
        if self.q.back().is_some_and(|p| p.stream == 0) {
            self.trim();
        }
    }

    /// Drops whole groups of pictures (with the sound that came with them) from the front
    /// while what remains is still long enough, or while the memory limit is exceeded.
    fn trim(&mut self) {
        loop {
            let Some(second_key) = self.q.iter().skip(1).position(Pkt::is_video_key).map(|i| i + 1) else { return };
            let enough_history = self.last_us - self.q[second_key].us >= self.max_span;
            if !enough_history && self.bytes <= self.max_bytes {
                return;
            }
            for p in self.q.drain(..second_key) {
                self.bytes -= p.data.len();
            }
        }
    }

    /// The last `span_us` microseconds, starting at the nearest video keyframe at or before
    /// that point. Cheap: packets are shared, not copied.
    pub fn snapshot(&self, span_us: i64) -> Vec<Pkt> {
        let from = self.last_us - span_us;
        let start = self.q.iter().rposition(|p| p.is_video_key() && p.us <= from).unwrap_or(0);
        self.q.iter().skip(start).cloned().collect()
    }
}
