// src-tauri/src/providers/sse.rs
//
// items.id=715 (decisions.id=875, 883; PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md
// Part 8i): an incremental server-sent-events parser. PURE: no I/O, no
// logging, no provider or dialect knowledge. The hosted client feeds it the
// raw bytes of a response body as they arrive and gets whole events back.
//
// What it handles (each pinned by a test):
//   - lines ended by LF, CRLF or a lone CR (a CR at the end of a chunk waits
//     for the next byte to see whether an LF follows);
//   - a character split across chunks: bytes are only decoded once a whole
//     line is available, so a split multi-byte character is never corrupted;
//   - a leading byte-order mark;
//   - `data` fields (several lines join with a newline), `event` fields; `id`,
//     `retry` and unknown fields are ignored; comment lines (keepalives such as
//     ": PROCESSING") are ignored; a field line with no colon; one space after
//     the colon is dropped;
//   - an event with no data is skipped.
//
// HOSTILE INPUT: memory is bounded. A line, and an event's data and name
// together, may not exceed `MAX_EVENT_BYTES`; exceeding either returns an
// error and the parser stays failed. Nothing here indexes unchecked or
// panics for any byte sequence (a seeded byte-soak test pins that).
//
// At a clean end of stream `finish` dispatches an event that has data but no
// closing blank line (some servers stop right after `data: [DONE]`). Whether
// the stream was COMPLETE is the dialect decoder's decision, never this
// module's.

/// The most bytes one line, or one event's data plus name, may hold.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;

/// One dispatched event.
#[derive(Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the event set one.
    pub event: Option<String>,
    /// The `data:` lines joined with a newline.
    pub data: String,
}

// The payload is a model's text on the streaming path, so Debug never prints
// it (decisions.id=883: no delta text in any log).
impl std::fmt::Debug for SseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseEvent")
            .field("event", &self.event)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseError {
    /// A single line grew past the cap without a line ending.
    LineTooLong,
    /// One event's data (and name) grew past the cap.
    EventTooLarge,
}

pub struct SseParser {
    max: usize,
    line: Vec<u8>,
    /// The previous byte was a CR that already ended a line.
    after_cr: bool,
    first_line: bool,
    data: String,
    has_data: bool,
    event: Option<String>,
    event_bytes: usize,
    failed: Option<SseError>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self::with_limit(MAX_EVENT_BYTES)
    }

    pub fn with_limit(max: usize) -> Self {
        Self {
            max,
            line: Vec::new(),
            after_cr: false,
            first_line: true,
            data: String::new(),
            has_data: false,
            event: None,
            event_bytes: 0,
            failed: None,
        }
    }

    /// Feed bytes; returns every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let mut out = Vec::new();
        for &b in bytes {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut out)?,
                b'\r' => {
                    self.end_line(&mut out)?;
                    self.after_cr = true;
                }
                _ => {
                    if self.line.len() >= self.max {
                        return Err(self.fail(SseError::LineTooLong));
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    /// The stream ended cleanly: handle an unterminated last line and
    /// dispatch an event that has data.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, SseError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.process_line()?;
        }
        self.dispatch(&mut out);
        Ok(out)
    }

    fn fail(&mut self, e: SseError) -> SseError {
        self.failed = Some(e);
        self.line.clear();
        self.data.clear();
        self.event = None;
        e
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        if self.line.is_empty() {
            // The blank line that closes an event. (The first-line BOM rule
            // cannot apply to an empty line.)
            self.first_line = false;
            self.dispatch(out);
            return Ok(());
        }
        self.process_line()
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.has_data {
            out.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data),
            });
        }
        self.event = None;
        self.data.clear();
        self.has_data = false;
        self.event_bytes = 0;
    }

    fn process_line(&mut self) -> Result<(), SseError> {
        let raw = std::mem::take(&mut self.line);
        let mut bytes: &[u8] = &raw;
        if self.first_line {
            self.first_line = false;
            if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
                bytes = rest;
            }
        }
        if bytes.is_empty() || bytes[0] == b':' {
            return Ok(());
        }
        let text = String::from_utf8_lossy(bytes);
        let (field, value) = match text.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (text.as_ref(), ""),
        };
        match field {
            "data" => {
                // +1 for the joining newline.
                self.event_bytes = self.event_bytes.saturating_add(value.len() + 1);
                if self.event_bytes > self.max {
                    return Err(self.fail(SseError::EventTooLarge));
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => {
                self.event_bytes = self.event_bytes.saturating_add(value.len());
                if self.event_bytes > self.max {
                    return Err(self.fail(SseError::EventTooLarge));
                }
                self.event = Some(value.to_owned());
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(input: &[u8]) -> Vec<(Option<String>, String)> {
        let mut p = SseParser::new();
        let mut evs = p.push(input).unwrap();
        evs.extend(p.finish().unwrap());
        evs.into_iter().map(|e| (e.event, e.data)).collect()
    }

    fn d(s: &str) -> (Option<String>, String) {
        (None, s.to_owned())
    }

    #[test]
    fn line_endings_lf_crlf_and_cr_are_equivalent() {
        let expect = vec![d("a"), d("b")];
        assert_eq!(all(b"data: a\n\ndata: b\n\n"), expect);
        assert_eq!(all(b"data: a\r\n\r\ndata: b\r\n\r\n"), expect);
        assert_eq!(all(b"data: a\r\rdata: b\r\r"), expect);
        // Mixed endings.
        assert_eq!(all(b"data: a\r\n\ndata: b\n\r\n"), expect);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        assert_eq!(
            all(b"data: one\ndata: two\ndata:three\n\n"),
            vec![d("one\ntwo\nthree")]
        );
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored_and_event_name_is_kept() {
        let evs = all(b": PROCESSING\nid: 7\nretry: 100\nfoo: bar\nevent: ping\ndata: {}\n\n");
        assert_eq!(evs, vec![(Some("ping".to_owned()), "{}".to_owned())]);
    }

    #[test]
    fn an_event_without_data_is_skipped_and_resets_the_name() {
        let evs = all(b"event: x\n\ndata: y\n\n");
        assert_eq!(evs, vec![d("y")]);
    }

    #[test]
    fn only_one_leading_space_is_dropped_and_a_bare_field_is_empty() {
        assert_eq!(all(b"data:  two spaces\n\n"), vec![d(" two spaces")]);
        assert_eq!(all(b"data\n\n"), vec![d("")]);
        assert_eq!(all(b"data:\n\n"), vec![d("")]);
    }

    #[test]
    fn a_leading_byte_order_mark_is_skipped_once() {
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"data: a\n\n");
        assert_eq!(all(&input), vec![d("a")]);
    }

    #[test]
    fn an_unterminated_final_event_is_dispatched_at_clean_end() {
        assert_eq!(all(b"data: [DONE]"), vec![d("[DONE]")]);
        assert_eq!(all(b"data: [DONE]\n"), vec![d("[DONE]")]);
    }

    #[test]
    fn every_two_way_split_gives_the_same_events() {
        let doc = "data: caf\u{e9} \u{1F600}\r\n\r\n: ka\r\ndata: a\r\ndata: b\r\n\r\nevent: e\ndata: z\n\n";
        let whole = all(doc.as_bytes());
        assert_eq!(whole.len(), 3);
        for i in 0..=doc.len() {
            let mut p = SseParser::new();
            let mut got = p.push(&doc.as_bytes()[..i]).unwrap();
            got.extend(p.push(&doc.as_bytes()[i..]).unwrap());
            got.extend(p.finish().unwrap());
            let got: Vec<_> = got.into_iter().map(|e| (e.event, e.data)).collect();
            assert_eq!(got, whole, "split at byte {i}");
        }
    }

    #[test]
    fn byte_at_a_time_feed_gives_the_same_events() {
        let doc = "data: h\u{e9}llo \u{4E16}\u{754C}\r\n\r\ndata: [DONE]\r\n\r\n";
        let whole = all(doc.as_bytes());
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for b in doc.as_bytes() {
            got.extend(p.push(std::slice::from_ref(b)).unwrap());
        }
        got.extend(p.finish().unwrap());
        let got: Vec<_> = got.into_iter().map(|e| (e.event, e.data)).collect();
        assert_eq!(got, whole);
        assert_eq!(got[0].1, "h\u{e9}llo \u{4E16}\u{754C}");
    }

    #[test]
    fn a_cr_at_a_chunk_end_does_not_double_count_the_following_lf() {
        let mut p = SseParser::new();
        let mut got = p.push(b"data: a\r").unwrap();
        got.extend(p.push(b"\n\r").unwrap());
        got.extend(p.push(b"\ndata: b\r\n\r\n").unwrap());
        let got: Vec<_> = got.into_iter().map(|e| e.data).collect();
        assert_eq!(got, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let evs = all(b"data: a\xFFb\n\n");
        assert_eq!(evs.len(), 1);
        assert!(evs[0].1.contains('\u{FFFD}'));
    }

    #[test]
    fn a_line_over_the_cap_fails_and_stays_failed() {
        let mut p = SseParser::with_limit(16);
        assert_eq!(p.push(&[b'x'; 17]).unwrap_err(), SseError::LineTooLong);
        assert_eq!(p.push(b"data: a\n\n").unwrap_err(), SseError::LineTooLong);
        assert_eq!(p.finish().unwrap_err(), SseError::LineTooLong);
    }

    #[test]
    fn an_event_over_the_cap_fails_even_when_each_line_is_small() {
        let mut p = SseParser::with_limit(32);
        let mut err = None;
        for _ in 0..10 {
            if let Err(e) = p.push(b"data: 0123456789\n") {
                err = Some(e);
                break;
            }
        }
        assert_eq!(err, Some(SseError::EventTooLarge));
    }

    #[test]
    fn many_small_events_do_not_accumulate_toward_the_cap() {
        let mut p = SseParser::with_limit(32);
        for _ in 0..1000 {
            let evs = p.push(b"data: 0123456789\n\n").unwrap();
            assert_eq!(evs.len(), 1);
        }
    }

    #[test]
    fn debug_never_prints_the_data() {
        let e = SseEvent {
            event: None,
            data: "secret reply text".to_owned(),
        };
        assert!(!format!("{e:?}").contains("secret"));
    }

    /// Hostile and random bytes never panic and never exceed the caps.
    #[test]
    fn a_seeded_byte_soak_never_panics() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Biased toward the bytes the parser treats specially.
        let alphabet: &[u8] = b"\r\n:data event\xEF\xBB\xBF\xFF\x00x";
        for _ in 0..300 {
            let mut p = SseParser::with_limit(64);
            let len = (next() % 400) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let mut pos = 0;
            while pos < bytes.len() {
                let step = 1 + (next() % 9) as usize;
                let end = (pos + step).min(bytes.len());
                if p.push(&bytes[pos..end]).is_err() {
                    break;
                }
                pos = end;
            }
            let _ = p.finish();
        }
    }
}
