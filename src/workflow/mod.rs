//! Workflows: the commit Run, resolve, the setup wizard, confirmation menus,
//! deterministic grouping, input primitives. These modules compose the
//! lower layers (git, llm, render) into user-facing flows.

use crate::llm::redact;
use crate::render::display::Display;
use crate::render::timeline::{Failure, StepIdx, driver::RunFrame};

pub mod confirm;
pub mod grouping;
pub mod input;
pub mod resolve;
pub mod review;
pub mod run;
pub mod setup;
pub mod undo;

/// The Redaction Gate step shared by the commit Run's payload sites and the
/// resolve workflow (issues #155/#159, ADR 0017): activate the step (the
/// policy has none when off), read the per-file payloads, gate them — a
/// refusal is the step's failed screen (ADR 0018), clean completes it.
fn redact_gate<P: AsRef<[(String, String)]>>(
    frame: &RunFrame,
    display: &Display,
    step: Option<StepIdx>,
    override_hint: redact::Override,
    pairs: impl FnOnce(StepIdx) -> anyhow::Result<P>,
) -> anyhow::Result<()> {
    if let Some(step) = step {
        frame.activate(step);
        let pairs = pairs(step)?;
        if let Err(e) = redact::gate(pairs.as_ref(), override_hint) {
            frame.fail(
                display,
                step,
                Failure::from_error("secret-like content detected", &e),
            );
            return Err(e);
        }
        frame.complete(display, step, "Redaction gate · clean");
    }
    Ok(())
}
