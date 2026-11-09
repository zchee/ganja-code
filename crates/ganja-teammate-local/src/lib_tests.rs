use ganja_core::teammate::{BACKENDS, parse_backend};
use ganja_protocol::team::MemberBackend;

use super::*;

/// Every named local backend is assembled here, and non-local entries are not.
/// The engine owns in-process members; cloud members are configured separately.
#[test]
fn every_named_local_backend_and_only_local_backends_are_assembled() {
    let assembled = backends(PaneShell::default(), PaneShare::default());

    for name in BACKENDS {
        let parsed =
            parse_backend(name).unwrap_or_else(|error| panic!("{name:?} should parse: {error}"));

        let local = !matches!(parsed, MemberBackend::InProcess | MemberBackend::Cloud);
        assert_eq!(
            assembled.of(parsed).is_some(),
            local,
            "{name:?} must be assembled if and only if it is a local backend"
        );
    }
}
