//! Repairs escape sequences the terminal byte parser split at a read
//! boundary (**D516**).
//!
//! crossterm commits a lone `ESC` the moment it is the last byte of a read
//! with no more input pending (`crossterm-0.29.0/src/event/sys/unix/parse.rs`:
//! a buffer of exactly `ESC` becomes a bare Esc key on the spot), so a Left
//! arrow whose `ESC [ D` bytes straddle two reads — tmux, ssh and pty
//! scheduling all produce such boundaries — arrives as a phantom Esc plus the
//! literal characters `[` and `D`. The phantom Esc cancels a running turn or
//! arms the backtrack walk, and the literals land in the composer as text:
//! `Explain[D this[D codebase`.
//!
//! This machine sits between the `EventStream` and [`crate::app::App`]'s
//! handler and undoes the split: a bare Esc is held for `HOLDOFF`, and if
//! the characters that follow spell a CSI or SS3 continuation, the key
//! crossterm would have produced unsplit is dispatched in their place.
//! Anything that is not a continuation releases the held Esc, then everything
//! accumulated, in arrival order — nothing is invented and nothing a person
//! typed is lost. A genuine Esc is therefore never dropped, only delayed by
//! at most the hold-off, which is well inside the backtrack gesture's 500ms
//! window and imperceptible against the render loop's own 16ms cadence.
//!
//! Two complete-but-unmappable sequences are deliberately **swallowed**
//! rather than replayed as text: a split SGR mouse fragment (a lost click
//! beats pasted garbage) and a split bracketed-paste marker (the paste body
//! then arrives as typed characters — exactly what a terminal without
//! bracketed paste delivers). Both leave a `tracing::debug!` line naming the
//! sequence. The same neutralize-don't-forward posture the shim pane door's
//! `paste_body` takes with control fragments, applied in the other direction.
//!
//! Sans-io like the `tmux` crate's parser: the caller passes `now` in and
//! asks for the next [`EscRepair::deadline`]; nothing here reads a clock or a
//! terminal. When the kitty keyboard protocol is active (**D517**) the
//! ambiguity this machine repairs cannot occur — Esc itself arrives as
//! `CSI 27 u` — so the machine is constructed in [`EscRepair::passthrough`]
//! mode and holds nothing.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};

/// How long a bare Esc waits for its `[` or `O` continuation.
///
/// The continuation, when the Esc really was a split sequence, arrives in the
/// very next read — microseconds to single-digit milliseconds behind. 25ms is
/// vim's `ttimeoutlen` neighborhood: far above any pty scheduling gap, far
/// below what a person perceives on a deliberate Esc press.
const HOLDOFF: Duration = Duration::from_millis(25);

/// How long a whole sequence may take from the Esc to its final byte.
///
/// The continuation characters can themselves straddle further reads
/// (`ESC` + `[1;` + `5D`), so the sequence deadline is longer than the first
/// byte's — but bounded, so a stream that opens like a sequence and never
/// finishes cannot hold input hostage.
const SEQ_DEADLINE: Duration = Duration::from_millis(50);

/// The longest accumulated continuation entertained before giving up.
///
/// Real navigation sequences are under ten characters; sixteen is generous.
/// Past it the accumulation is released literally rather than grown forever.
const MAX_SEQ: usize = 16;

/// What is currently held back from dispatch.
#[derive(Debug)]
enum State {
    /// Nothing held; everything flows through.
    Idle,
    /// A bare Esc held since `since`, waiting for a `[` or `O`.
    Held { esc: KeyEvent, since: Instant },
    /// The opener arrived; accumulating the rest of the sequence.
    ///
    /// `keys` holds the original events — opener included — so a mismatch or
    /// expiry replays exactly what arrived, modifiers and all.
    Seq {
        esc: KeyEvent,
        since: Instant,
        kind: SeqKind,
        keys: Vec<KeyEvent>,
    },
}

/// Which escape-sequence family the opener announced.
#[derive(Clone, Copy, Debug)]
enum SeqKind {
    /// `ESC [` — parameters, then a final byte.
    Csi,
    /// `ESC O` — exactly one final byte.
    Ss3,
}

/// The hold-off state machine; see the module doc.
#[derive(Debug)]
pub struct EscRepair {
    /// `true` leaves every event untouched — the kitty protocol has already
    /// made Esc unambiguous, and holding it would be pure latency.
    passthrough: bool,
    state: State,
}

impl EscRepair {
    /// A machine that repairs splits: bare Escs are held per `HOLDOFF`.
    pub fn active() -> Self {
        Self {
            passthrough: false,
            state: State::Idle,
        }
    }

    /// A machine that holds nothing, for terminals speaking the kitty
    /// keyboard protocol (**D517**) where the ambiguity cannot occur.
    pub fn passthrough() -> Self {
        Self {
            passthrough: true,
            state: State::Idle,
        }
    }

    /// Feeds one terminal event through; returns the events to dispatch now,
    /// in order. Usually one; zero while holding; more on a release.
    pub fn accept(&mut self, event: TermEvent, now: Instant) -> Vec<TermEvent> {
        if self.passthrough {
            return vec![event];
        }
        let mut out = Vec::new();
        // What is held past its deadline was never a continuation — release
        // it first, so a late `[` is text and not a sequence opener.
        self.flush_expired(now, &mut out);
        self.feed(event, now, &mut out);
        out
    }

    /// When the caller must call [`EscRepair::expire`] if no event arrives
    /// first. [`None`] while nothing is held.
    pub fn deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Idle => None,
            State::Held { since, .. } => Some(*since + HOLDOFF),
            State::Seq { since, .. } => Some(*since + SEQ_DEADLINE),
        }
    }

    /// The deadline passed with nothing further: releases what is held.
    pub fn expire(&mut self, now: Instant) -> Vec<TermEvent> {
        let mut out = Vec::new();
        self.flush_expired(now, &mut out);
        out
    }

    fn flush_expired(&mut self, now: Instant, out: &mut Vec<TermEvent>) {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            match std::mem::replace(&mut self.state, State::Idle) {
                State::Idle => {}
                State::Held { esc, .. } => out.push(TermEvent::Key(esc)),
                State::Seq { esc, keys, .. } => release(esc, keys, out),
            }
        }
    }

    fn feed(&mut self, event: TermEvent, now: Instant, out: &mut Vec<TermEvent>) {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => {
                if let Some(esc) = bare_esc(&event) {
                    self.state = State::Held { esc, since: now };
                } else {
                    out.push(event);
                }
            }
            State::Held { esc, since } => match continuation(&event) {
                Some(('[', key)) => {
                    self.state = State::Seq {
                        esc,
                        since,
                        kind: SeqKind::Csi,
                        keys: vec![key],
                    };
                }
                Some(('O', key)) => {
                    self.state = State::Seq {
                        esc,
                        since,
                        kind: SeqKind::Ss3,
                        keys: vec![key],
                    };
                }
                // Not an opener: the Esc was real. Release it, then let the
                // new event start over — it may itself be an Esc to hold.
                _ => {
                    out.push(TermEvent::Key(esc));
                    self.feed(event, now, out);
                }
            },
            State::Seq {
                esc,
                since,
                kind,
                mut keys,
            } => match (kind, continuation(&event)) {
                (SeqKind::Csi, Some((c, key))) if is_csi_param(c) => {
                    keys.push(key);
                    if keys.len() > MAX_SEQ {
                        release(esc, keys, out);
                    } else {
                        self.state = State::Seq {
                            esc,
                            since,
                            kind,
                            keys,
                        };
                    }
                }
                (SeqKind::Csi, Some((c, _))) if is_csi_final(c) => {
                    let params: String = keys[1..].iter().filter_map(key_char).collect();
                    match csi_key(&params, c) {
                        Some(repaired) => out.push(repaired),
                        None => {
                            tracing::debug!(sequence = %format!("CSI {params}{c}"), "swallowed an unmappable split sequence");
                        }
                    }
                }
                (SeqKind::Ss3, Some((c, _))) => match ss3_key(c) {
                    Some(repaired) => out.push(repaired),
                    None => {
                        tracing::debug!(sequence = %format!("SS3 {c}"), "swallowed an unmappable split sequence");
                    }
                },
                // A character outside the grammar, or not a character at all:
                // release everything in arrival order, then start over.
                _ => {
                    release(esc, keys, out);
                    self.feed(event, now, out);
                }
            },
        }
    }
}

/// Releases a held Esc and its accumulated continuation, in arrival order.
fn release(esc: KeyEvent, keys: Vec<KeyEvent>, out: &mut Vec<TermEvent>) {
    out.push(TermEvent::Key(esc));
    out.extend(keys.into_iter().map(TermEvent::Key));
}

/// A bare Esc press — the only thing ever held.
///
/// Modified or non-press Esc events pass through: the split only ever
/// produces the plain form, and `KeyEventKind::Release` is already ignored
/// downstream.
fn bare_esc(event: &TermEvent) -> Option<KeyEvent> {
    match event {
        TermEvent::Key(key)
            if key.code == KeyCode::Esc
                && key.modifiers == KeyModifiers::NONE
                && key.kind == KeyEventKind::Press =>
        {
            Some(*key)
        }
        _ => None,
    }
}

/// A character that could be part of a split sequence.
///
/// crossterm marks uppercase ASCII with `SHIFT`, so a final like `D` arrives
/// shifted; any other modifier means a person's chord, never a fragment.
fn continuation(event: &TermEvent) -> Option<(char, KeyEvent)> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char(c) if (key.modifiers & !KeyModifiers::SHIFT) == KeyModifiers::NONE => {
                Some((c, *key))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The character a continuation key carried.
fn key_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// CSI parameter (`0x30..=0x3F`) or intermediate (`0x20..=0x2F`) byte.
fn is_csi_param(c: char) -> bool {
    matches!(c, '\x20'..='\x3F')
}

/// CSI final byte (`0x40..=0x7E`).
fn is_csi_final(c: char) -> bool {
    matches!(c, '\x40'..='\x7E')
}

/// The event crossterm would have produced for `ESC [ <params> <final>`,
/// or [`None`] for a sequence not worth reconstructing.
fn csi_key(params: &str, final_char: char) -> Option<TermEvent> {
    match final_char {
        'A' | 'B' | 'C' | 'D' | 'H' | 'F' => {
            let code = cursor_key(final_char)?;
            let modifiers = match params.split(';').nth(1) {
                Some(raw) => parse_modifiers(raw)?,
                None => KeyModifiers::NONE,
            };
            Some(TermEvent::Key(KeyEvent::new(code, modifiers)))
        }
        // `ESC [ Z` is Shift+Tab, and crossterm spells the shift out.
        'Z' if params.is_empty() => Some(TermEvent::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        ))),
        'I' if params.is_empty() => Some(TermEvent::FocusGained),
        'O' if params.is_empty() => Some(TermEvent::FocusLost),
        '~' => {
            let mut parts = params.split(';');
            let number: u16 = parts.next()?.parse().ok()?;
            let modifiers = match parts.next() {
                Some(raw) => parse_modifiers(raw)?,
                None => KeyModifiers::NONE,
            };
            let code = match number {
                1 | 7 => KeyCode::Home,
                4 | 8 => KeyCode::End,
                2 => KeyCode::Insert,
                3 => KeyCode::Delete,
                5 => KeyCode::PageUp,
                6 => KeyCode::PageDown,
                // 200/201 are the bracketed-paste markers; everything else
                // here is nothing this composer binds. Swallowed, not text.
                _ => return None,
            };
            Some(TermEvent::Key(KeyEvent::new(code, modifiers)))
        }
        // SGR mouse (`<...M`/`<...m`), unknown finals: not reconstructed.
        _ => None,
    }
}

/// The cursor-cluster finals `CSI` and `SS3` spell the same way: the arrows,
/// Home and End. One map, so the two decoders cannot disagree about what a
/// final means — and so a final outside it is "not reconstructed" rather than
/// a default key.
fn cursor_key(final_char: char) -> Option<KeyCode> {
    match final_char {
        'A' => Some(KeyCode::Up),
        'B' => Some(KeyCode::Down),
        'C' => Some(KeyCode::Right),
        'D' => Some(KeyCode::Left),
        'H' => Some(KeyCode::Home),
        'F' => Some(KeyCode::End),
        _ => None,
    }
}

/// The event crossterm would have produced for `ESC O <final>`.
fn ss3_key(final_char: char) -> Option<TermEvent> {
    let code = cursor_key(final_char).or(match final_char {
        'P' => Some(KeyCode::F(1)),
        'Q' => Some(KeyCode::F(2)),
        'R' => Some(KeyCode::F(3)),
        'S' => Some(KeyCode::F(4)),
        _ => None,
    })?;
    Some(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

/// Decodes the xterm modifier parameter: the value minus one is a bitmask of
/// shift (1), alt (2) and ctrl (4) — exactly crossterm's own reading.
fn parse_modifiers(raw: &str) -> Option<KeyModifiers> {
    let value: u8 = raw.parse().ok()?;
    let mask = value.checked_sub(1)?;
    let mut modifiers = KeyModifiers::NONE;
    if mask & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if mask & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if mask & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some(modifiers)
}

#[cfg(test)]
#[path = "escrepair_tests.rs"]
mod tests;
