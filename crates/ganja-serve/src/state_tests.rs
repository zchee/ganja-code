use super::ServedDirectory;

#[test]
fn a_directory_matches_its_own_spellings_and_nothing_else() {
    let temp = std::env::temp_dir();
    let served = ServedDirectory::new(temp.clone());

    assert!(served.matches(&temp.display().to_string()));
    // The canonical spelling matches too — on macOS `/tmp` and
    // `/private/tmp` are the same place, and the guard must know it.
    assert!(
        served.matches(
            &temp
                .canonicalize()
                .expect("temp resolves")
                .display()
                .to_string()
        )
    );

    assert!(!served.matches("/nonexistent/elsewhere"));
    assert!(!served.matches(""));
}
