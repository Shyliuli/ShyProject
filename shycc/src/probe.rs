//! Opt-in const-fn probing.
//!
//! `--constfn-probe` is the only way in, and an ordinary compile never runs
//! anything. A probe is not the user's program: the compiler names the const-fn
//! calls it cannot fold, the driver links those calls into a separate wrapper
//! image and runs that, and the answers go back into the compiler for the next
//! round. The final `.sfs` is never executed.
//!
//! TODO(shyc): the compiler does not emit probe requests yet, so no round can
//! start. The contract the driver needs:
//!   * `shyc` reports the pending probe requests of one unit (which function,
//!     which arguments, which result type) instead of failing on a const fn
//!     whose arguments are not compile-time constants;
//!   * the driver links a wrapper image from those requests only: its own entry
//!     point, one call per request, the answers written as framed data;
//!   * the wrapper carries the emulator's probe marker, and the emulator's probe
//!     mode checks it before running anything: the driver alone cannot run a
//!     wrapper, and the emulator alone cannot pick one up;
//!   * the emulator runs the wrapper with an empty stdin and captured output and
//!     returns structured answers, never an exit code;
//!   * `shyc` takes the answers and recompiles until no request remains.
//!
//! The wrapper is a temporary of the same build: it is not the artifact, is
//! never written next to it, and goes away with the other temporaries.
//!
//! Until all of that exists this module fails closed: the flag builds nothing.

use anyhow::{Result, bail};

/// Run the probe rounds of this compilation.
///
/// TODO(shyc): replace the refusal with the round loop once `shyc` can emit
/// probe requests and take answers back.
pub fn rounds() -> Result<()> {
    bail!(
        "`--constfn-probe` is not wired up: `shyc` does not emit const-fn probe \
         requests yet, so no wrapper was built and nothing was executed"
    )
}
