use ganja_core::teammate::{BACKENDS, parse_backend};
use ganja_protocol::team::MemberBackend;

use super::*;

/// Restores, as a test, the exhaustiveness check `Backends` traded away
/// (plan ADR "Consequences", mitigated by U-6): every name
/// [`ganja_core::teammate::BACKENDS`] spells resolves through [`backends`],
/// except `in-process`, which is the engine's own entry and is never one this
/// crate assembles.
#[test]
fn every_named_backend_but_in_process_is_assembled() {
    let assembled = backends(PaneShell::default(), PaneShare::default());

    for name in BACKENDS {
        let parsed =
            parse_backend(name).unwrap_or_else(|error| panic!("{name:?} should parse: {error}"));

        if parsed == MemberBackend::InProcess {
            assert!(
                assembled.of(parsed).is_none(),
                "{name:?} is the engine's own entry and must not be assembled here"
            );
        } else {
            assert!(
                assembled.of(parsed).is_some(),
                "{name:?} has no backend implementation in `backends()`"
            );
        }
    }
}
