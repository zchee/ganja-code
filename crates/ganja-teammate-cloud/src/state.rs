//! Lifecycle outcomes are explicit: local retirement does not prove remote
//! deletion, and an outstanding request is not an uncertain completed attempt.

use std::time::Duration;

/// Overall launch budget, including resume, checkout, health, session and first stream ack.
pub const LAUNCH_DEADLINE: Duration = Duration::from_secs(420);
/// Checkout and status reconciliation share this budget inside launch.
pub const BOOTSTRAP_DEADLINE: Duration = Duration::from_secs(180);
/// Maximum wait for the authenticated engine health barrier.
pub const HEALTH_DEADLINE: Duration = Duration::from_secs(90);
/// Maximum wait for outstanding REST requests before deferring a suspend.
pub const QUIESCE_DEADLINE: Duration = Duration::from_secs(60);
/// Independent budget for one delete and one confirmation read.
pub const CLEANUP_DEADLINE: Duration = Duration::from_secs(60);
/// Silence budget for a terminal missing from the stream and stored envelope.
pub const TERMINAL_RECONCILE_DEADLINE: Duration = Duration::from_secs(120);

/// Current activity of a member. Uncertainty is an independent, permanent gate latch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Resource allocation is in progress.
    Creating,
    /// The init listener is preparing the checkout.
    Bootstrapping,
    /// Ready to accept a new inbox message.
    Idle,
    /// A prompt has been submitted but its acceptance is being resolved.
    Prompting,
    /// An assistant turn is in progress.
    Turning,
    /// Admission is closed while outstanding requests settle.
    Quiescing,
    /// A request remains outstanding; suspension will be retried at a later boundary.
    SuspendDeferred,
    /// The control plane is suspending the process.
    Suspending,
    /// The control plane confirmed suspension.
    Suspended,
    /// The process is resuming and reconnecting.
    Resuming,
    /// Launch failed; remote cleanup is running under a separate budget.
    Unwinding,
    /// The remote identity was confirmed absent.
    Gone,
    /// Local bookkeeping is released but remote deletion is unconfirmed.
    CleanupUnconfirmed,
    /// A turn cannot progress automatically without an explicit owner decision.
    RecoveryRequired,
    /// The control plane reports a crashed process.
    Crashed,
}

/// A typed reason why startup did not reach an authenticated connected worker.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchFailure {
    /// The first control-plane resume did not finish.
    #[error("launch timed out while resuming the worker")]
    ResumeTimedOut,
    /// The bootstrap's bounded status reconciliation did not finish.
    #[error("bootstrap timed out")]
    BootstrapTimedOut,
    /// The POST may still arrive; no second POST is permitted.
    #[error("startup is ambiguous; the worker must be deleted")]
    AmbiguousStartup,
    /// Init gave an authoritative failure; only its fixed stage is retained.
    #[error("bootstrap failed during {0}")]
    BootstrapFailed(&'static str),
    /// Init became reachable but the assembled engine did not.
    #[error("launch timed out waiting for authenticated health")]
    HealthTimedOut,
    /// The engine never answered session discovery or creation.
    #[error("launch timed out while opening a session")]
    SessionTimedOut,
    /// The first connected acknowledgement never arrived.
    #[error("launch timed out waiting for the event stream")]
    ConnectTimedOut,
    /// A bounded operation gave an authoritative non-success answer.
    #[error("launch failed during {0}")]
    Refused(&'static str),
}

/// Budgets kept in one value so tests can use short waits without changing production constants.
#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    /// Entire launch.
    pub launch: Duration,
    /// Checkout and bounded status reads within launch.
    pub bootstrap: Duration,
    /// Authenticated health within launch.
    pub health: Duration,
    /// REST drain at a turn boundary.
    pub quiesce: Duration,
    /// One delete and one confirmation read.
    pub cleanup: Duration,
    /// Awaiting a missing terminal event.
    pub terminal: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            launch: LAUNCH_DEADLINE,
            bootstrap: BOOTSTRAP_DEADLINE,
            health: HEALTH_DEADLINE,
            quiesce: QUIESCE_DEADLINE,
            cleanup: CLEANUP_DEADLINE,
            terminal: TERMINAL_RECONCILE_DEADLINE,
        }
    }
}
