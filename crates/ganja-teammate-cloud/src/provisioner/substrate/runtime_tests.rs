use super::super::ateapi_v1::ActorStatus;
use super::*;

#[test]
fn every_wire_state_maps_to_a_conservative_worker_state() {
    for (state, expected) in [
        (ActorState::Running, WorkerState::Running),
        (ActorState::Suspended, WorkerState::Suspended),
        (ActorState::Crashed, WorkerState::Crashed),
        (ActorState::Resuming, WorkerState::Transitioning),
        (ActorState::Suspending, WorkerState::Transitioning),
        (ActorState::Deleting, WorkerState::Transitioning),
        (ActorState::Reverting, WorkerState::Transitioning),
        (ActorState::Unspecified, WorkerState::Transitioning),
    ] {
        let actor = Actor {
            status: Some(ActorStatus { state: state.into(), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(actor_state(&actor), expected);
    }
    assert_eq!(actor_state(&Actor::default()), WorkerState::Transitioning);
}

#[test]
fn mutation_transport_failures_are_ambiguous_and_remote_bodies_are_discarded() {
    for code in [
        Code::Unavailable,
        Code::DeadlineExceeded,
        Code::Cancelled,
        Code::Unknown,
        Code::Internal,
        Code::DataLoss,
        Code::Aborted,
        Code::ResourceExhausted,
    ] {
        let status = Status::new(code, "synthetic-provider-secret-canary");
        let mutation = rpc_error("mutate", &status, true);
        assert_eq!(mutation.kind, ProvisionErrorKind::Ambiguous);
        assert_eq!(rpc_error("read", &status, false).kind, ProvisionErrorKind::Transient);
        assert!(!format!("{mutation:?} {mutation}").contains("synthetic-provider-secret-canary"));
    }
    assert_eq!(
        rpc_error("read", &Status::not_found("canary"), false).kind,
        ProvisionErrorKind::Gone
    );
    assert_eq!(
        rpc_error("read", &Status::permission_denied("canary"), false).kind,
        ProvisionErrorKind::Refused
    );
}
