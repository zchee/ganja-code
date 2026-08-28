use super::*;

/// A char key the way crossterm delivers it: uppercase carries SHIFT.
fn ch(c: char) -> TermEvent {
    let modifiers = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
    TermEvent::Key(KeyEvent::new(KeyCode::Char(c), modifiers))
}

fn esc() -> TermEvent {
    TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent::new(code, modifiers))
}

/// Feeds a burst of events 1ms apart and returns everything dispatched.
fn run(machine: &mut EscRepair, events: &[TermEvent], base: Instant) -> Vec<TermEvent> {
    let mut out = Vec::new();
    for (i, event) in events.iter().enumerate() {
        let now = base + Duration::from_millis(i as u64);
        out.extend(machine.accept(event.clone(), now));
    }
    out
}

#[test]
fn a_split_left_arrow_becomes_left_and_never_text() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('['), ch('D')], Instant::now());
    assert_eq!(out, vec![key(KeyCode::Left, KeyModifiers::NONE)]);
    assert_eq!(machine.deadline(), None);
}

#[test]
fn a_modified_arrow_keeps_its_modifiers() {
    let mut machine = EscRepair::active();
    let out =
        run(&mut machine, &[esc(), ch('['), ch('1'), ch(';'), ch('5'), ch('D')], Instant::now());
    assert_eq!(out, vec![key(KeyCode::Left, KeyModifiers::CONTROL)]);
}

#[test]
fn a_tilde_sequence_maps_its_number() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('['), ch('3'), ch('~')], Instant::now());
    assert_eq!(out, vec![key(KeyCode::Delete, KeyModifiers::NONE)]);
}

#[test]
fn shift_tab_survives_the_split() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('['), ch('Z')], Instant::now());
    assert_eq!(out, vec![key(KeyCode::BackTab, KeyModifiers::SHIFT)]);
}

#[test]
fn an_ss3_arrow_maps_like_csi() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('O'), ch('D')], Instant::now());
    assert_eq!(out, vec![key(KeyCode::Left, KeyModifiers::NONE)]);
}

#[test]
fn a_split_focus_event_is_still_a_focus_event() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('['), ch('I')], Instant::now());
    assert_eq!(out, vec![TermEvent::FocusGained]);
}

#[test]
fn a_bare_esc_is_released_at_the_deadline() {
    let mut machine = EscRepair::active();
    let base = Instant::now();
    assert!(machine.accept(esc(), base).is_empty());
    let deadline = machine.deadline().expect("a held esc has a deadline");
    assert_eq!(deadline, base + HOLDOFF);
    // A wake just before the deadline releases nothing.
    assert!(machine.expire(deadline - Duration::from_millis(1)).is_empty());
    assert_eq!(machine.expire(deadline), vec![esc()]);
    assert_eq!(machine.deadline(), None);
}

#[test]
fn esc_esc_releases_the_first_and_holds_the_second() {
    let mut machine = EscRepair::active();
    let base = Instant::now();
    assert!(machine.accept(esc(), base).is_empty());
    let out = machine.accept(esc(), base + Duration::from_millis(5));
    assert_eq!(out, vec![esc()]);
    // The second is now held, released at its own deadline.
    assert_eq!(machine.expire(base + Duration::from_millis(5) + HOLDOFF), vec![esc()]);
}

#[test]
fn a_mismatch_releases_everything_in_arrival_order() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[esc(), ch('a')], Instant::now());
    assert_eq!(out, vec![esc(), ch('a')]);
}

#[test]
fn a_mismatch_mid_sequence_replays_the_fragment_literally() {
    let mut machine = EscRepair::active();
    // `[1` then a ctrl-chord: not a sequence after all.
    let chord = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let out = run(&mut machine, &[esc(), ch('['), ch('1'), chord.clone()], Instant::now());
    assert_eq!(out, vec![esc(), ch('['), ch('1'), chord]);
}

#[test]
fn a_late_continuation_is_text_not_a_key() {
    let mut machine = EscRepair::active();
    let base = Instant::now();
    assert!(machine.accept(esc(), base).is_empty());
    // The `[` arrives after the hold-off: the esc was real, the bracket
    // is text.
    let out = machine.accept(ch('['), base + HOLDOFF + Duration::from_millis(1));
    assert_eq!(out, vec![esc(), ch('[')]);
}

#[test]
fn a_runaway_sequence_is_released_at_the_cap() {
    let mut machine = EscRepair::active();
    let base = Instant::now();
    let mut events = vec![esc(), ch('[')];
    events.extend(std::iter::repeat_n(ch('1'), MAX_SEQ));
    let out = run(&mut machine, &events, base);
    assert_eq!(out.len(), events.len());
    assert_eq!(out[0], esc());
    assert_eq!(machine.deadline(), None);
}

#[test]
fn a_split_mouse_fragment_is_swallowed() {
    let mut machine = EscRepair::active();
    let out = run(
        &mut machine,
        &[esc(), ch('['), ch('<'), ch('0'), ch(';'), ch('5'), ch(';'), ch('3'), ch('M')],
        Instant::now(),
    );
    assert_eq!(out, vec![]);
    assert_eq!(machine.deadline(), None);
}

#[test]
fn a_split_paste_marker_is_swallowed() {
    let mut machine = EscRepair::active();
    let out =
        run(&mut machine, &[esc(), ch('['), ch('2'), ch('0'), ch('0'), ch('~')], Instant::now());
    assert_eq!(out, vec![]);
}

#[test]
fn passthrough_holds_nothing() {
    let mut machine = EscRepair::passthrough();
    let base = Instant::now();
    assert_eq!(machine.accept(esc(), base), vec![esc()]);
    assert_eq!(machine.deadline(), None);
    assert_eq!(machine.accept(ch('['), base), vec![ch('[')]);
    assert_eq!(machine.accept(ch('D'), base), vec![ch('D')]);
}

#[test]
fn a_release_kind_key_passes_through() {
    let mut machine = EscRepair::active();
    let mut released = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    released.kind = KeyEventKind::Release;
    let event = TermEvent::Key(released);
    assert_eq!(machine.accept(event.clone(), Instant::now()), vec![event]);
    assert_eq!(machine.deadline(), None);
}

#[test]
fn a_non_key_event_while_holding_releases_first() {
    let mut machine = EscRepair::active();
    let base = Instant::now();
    assert!(machine.accept(esc(), base).is_empty());
    let resize = TermEvent::Resize(80, 24);
    let out = machine.accept(resize.clone(), base + Duration::from_millis(2));
    assert_eq!(out, vec![esc(), resize]);
}

#[test]
fn an_ordinary_key_flows_through_untouched() {
    let mut machine = EscRepair::active();
    let out = run(&mut machine, &[ch('a'), ch('['), ch('D')], Instant::now());
    // No esc in front: `[` and `D` are just text.
    assert_eq!(out, vec![ch('a'), ch('['), ch('D')]);
}
