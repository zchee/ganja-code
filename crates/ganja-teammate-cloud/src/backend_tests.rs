use super::*;

#[test]
fn repositories_keep_their_original_path_and_configured_host() {
    let cases = [
        ("https://example.invalid/repo.git", "example.invalid", true),
        ("https://EXAMPLE.invalid/a/repo.git", "EXAMPLE.INVALID", true),
        ("https://127.0.0.1/repo.git", "127.0.0.1", true),
        ("https://[::1]/repo.git", "[::1]", false),
        ("https://example.invalid/a/./repo.git", "example.invalid", false),
        ("https://example.invalid/a/../repo.git", "example.invalid", false),
        ("https://example.invalid/a/%2e/repo.git", "example.invalid", false),
        ("https://example.invalid/a/%2E%2e/repo.git", "example.invalid", false),
        ("https://example.invalid/a/.%2E/repo.git", "example.invalid", false),
        ("https://example.invalid/a/%2e./repo.git", "example.invalid", false),
        ("https://example.invalid/a\\repo.git", "example.invalid", false),
        ("https://example.invalid/a/\nrepo.git", "example.invalid", false),
        ("https://example.invalid/a/%2e%2e", "example.invalid", false),
        ("https://example.invalid/.git", "example.invalid", true),
        ("https://example.invalid/a..b/repo.git", "example.invalid", true),
        ("https://other.invalid/repo.git", "example.invalid", false),
        ("https://example.invalid.evil/repo.git", "example.invalid", false),
    ];
    for (repository, host, expected) in cases {
        assert_eq!(valid_repository(repository, host), expected, "{repository}");
    }
}
