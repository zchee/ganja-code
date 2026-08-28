use super::*;

#[test]
fn an_id_converts_into_an_argv_word_owned_or_borrowed() {
    let pane = PaneId::new("%12").expect("a well-formed pane id");
    assert_eq!(std::ffi::OsString::from(&pane), std::ffi::OsString::from("%12"));
    assert_eq!(std::ffi::OsString::from(pane), std::ffi::OsString::from("%12"));

    let window = WindowId::new("@3").expect("a well-formed window id");
    assert_eq!(std::ffi::OsString::from(window), std::ffi::OsString::from("@3"));

    let session = SessionId::new("$4").expect("a well-formed session id");
    assert_eq!(std::ffi::OsString::from(session), std::ffi::OsString::from("$4"));
}

#[test]
fn a_pane_id_without_the_percent_prefix_is_refused() {
    let err = PaneId::new("1").unwrap_err();
    assert!(err.to_string().contains("pane ID"));
}

#[test]
fn a_pane_id_with_only_the_percent_prefix_is_refused() {
    let err = PaneId::new("%").unwrap_err();
    assert!(err.to_string().contains("pane ID"));
}

#[test]
fn a_pane_id_with_a_non_digit_after_the_prefix_is_refused() {
    let err = PaneId::new("%a").unwrap_err();
    assert!(err.to_string().contains("decimal digits"));
}

#[test]
fn a_well_formed_pane_id_round_trips_through_as_str() {
    let pane = PaneId::new("%12").unwrap();
    assert_eq!(pane.as_str(), "%12");
    assert_eq!(pane.to_string(), "%12");
}

#[test]
fn a_window_id_without_the_at_prefix_is_refused() {
    let err = WindowId::new("1").unwrap_err();
    assert!(err.to_string().contains("window ID"));
}

#[test]
fn a_well_formed_window_id_round_trips_through_as_str() {
    let window = WindowId::new("@3").unwrap();
    assert_eq!(window.as_str(), "@3");
}

#[test]
fn a_session_id_without_the_dollar_prefix_is_refused() {
    let err = SessionId::new("1").unwrap_err();
    assert!(err.to_string().contains("session ID"));
}

#[test]
fn a_well_formed_session_id_round_trips_through_as_str() {
    let session = SessionId::new("$4").unwrap();
    assert_eq!(session.as_str(), "$4");
}
