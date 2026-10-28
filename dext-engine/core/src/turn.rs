//! Shared turn-loop control. Native preparation, policies, grants and I/O remain
//! explicit host work; this cursor alone is not a complete recoverable agent turn.
use crate::recovery::{Decision, HaltReason, Observation as RecoveryObservation, RecoveryState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Ready,
    Preparing,
    ModelWait,
    ToolsWait,
    FinalReview,
    AfterTools,
    Stopped(Stop),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stop {
    Complete,
    Iterations,
    Budget,
    ToolPolicy,
    ContentFilter,
    RecoveryBudget,
    Cancelled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    Exhausted,
    Encoding,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "turn control: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnControl {
    phase: Phase,
    iterations: u32,
    request_ordinal: u64,
    recovery: RecoveryState,
}
impl Default for TurnControl {
    fn default() -> Self {
        Self {
            phase: Phase::Ready,
            iterations: 0,
            request_ordinal: 0,
            recovery: RecoveryState::default(),
        }
    }
}
impl TurnControl {
    pub fn phase(self) -> Phase {
        self.phase
    }
    pub fn iterations(self) -> u32 {
        self.iterations
    }
    pub fn request_ordinal(self) -> u64 {
        self.request_ordinal
    }
    pub fn recovery(self) -> RecoveryState {
        self.recovery
    }

    /// Proposal only: no model/tool may be dispatched from an uncommitted cursor.
    pub fn begin_round(self, limit: Option<u32>) -> Result<Self, Error> {
        if !matches!(self.phase, Phase::Ready | Phase::AfterTools) {
            return Err(Error::State);
        }
        if limit.is_some_and(|limit| self.iterations >= limit) {
            return self.stop(Stop::Iterations);
        }
        Ok(Self {
            phase: Phase::Preparing,
            iterations: self.iterations.checked_add(1).ok_or(Error::Exhausted)?,
            ..self
        })
    }

    /// Rebuilding a failed stream is a new logical request ordinal, not a retry of
    /// a recorded response. Physical HTTP attempts remain in ModelWait.
    pub fn prepare_request(self) -> Result<Self, Error> {
        if !matches!(self.phase, Phase::Preparing | Phase::ModelWait) {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::ModelWait,
            request_ordinal: self
                .request_ordinal
                .checked_add(1)
                .ok_or(Error::Exhausted)?,
            ..self
        })
    }

    pub fn reset_recovery(self) -> Result<Self, Error> {
        if matches!(self.phase, Phase::Stopped(_)) {
            return Err(Error::State);
        }
        Ok(Self {
            recovery: self.recovery.advance(RecoveryObservation::Reset).next,
            ..self
        })
    }

    /// Host confirms the old stream/preparation was abandoned and cleaned up.
    pub fn runtime_control(self) -> Result<Self, Error> {
        if !matches!(self.phase, Phase::Preparing | Phase::ModelWait) {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::Ready,
            ..self.reset_recovery()?
        })
    }

    pub fn response(
        self,
        incomplete_reason: Option<&str>,
        executable_calls: bool,
    ) -> Result<(Self, Decision), Error> {
        if self.phase != Phase::ModelWait {
            return Err(Error::State);
        }
        let transition = self.recovery.advance(RecoveryObservation::Response {
            incomplete_reason,
            executable_calls,
        });
        let phase = match transition.decision {
            Decision::Proceed if executable_calls => Phase::ToolsWait,
            Decision::Proceed => Phase::FinalReview,
            Decision::Retry { .. } => Phase::Ready,
            Decision::Halt(HaltReason::ContentFilter) => Phase::Stopped(Stop::ContentFilter),
            Decision::Halt(HaltReason::RetryBudget) => Phase::Stopped(Stop::RecoveryBudget),
        };
        let iterations = if matches!(transition.decision, Decision::Retry { .. }) {
            self.iterations.checked_sub(1).ok_or(Error::State)?
        } else {
            self.iterations
        };
        Ok((
            Self {
                phase,
                iterations,
                recovery: transition.next,
                ..self
            },
            transition.decision,
        ))
    }
    /// An explicit host decision to keep working after the step limit: the cursor
    /// returns to Ready with a fresh step count; nothing was in flight at the stop.
    pub fn extend_iterations(self) -> Result<Self, Error> {
        if self.phase != Phase::Stopped(Stop::Iterations) {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::Ready,
            iterations: 0,
            ..self
        })
    }
    pub fn tools_applied(self) -> Result<Self, Error> {
        if self.phase != Phase::ToolsWait {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::AfterTools,
            ..self
        })
    }
    pub fn task_followup(self) -> Result<Self, Error> {
        if self.phase != Phase::Stopped(Stop::Complete) {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::Ready,
            ..self
        })
    }

    pub fn final_followup(self) -> Result<Self, Error> {
        if self.phase != Phase::FinalReview {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::Ready,
            ..self
        })
    }
    pub fn finish(self) -> Result<Self, Error> {
        if self.phase != Phase::FinalReview {
            return Err(Error::State);
        }
        self.stop(Stop::Complete)
    }
    pub fn stop(self, stop: Stop) -> Result<Self, Error> {
        let allowed = match stop {
            Stop::Complete => self.phase == Phase::FinalReview,
            Stop::Iterations => matches!(self.phase, Phase::Ready | Phase::AfterTools),
            Stop::ToolPolicy => matches!(self.phase, Phase::ToolsWait | Phase::AfterTools),
            Stop::Budget | Stop::Cancelled => !matches!(self.phase, Phase::Stopped(_)),
            Stop::ContentFilter | Stop::RecoveryBudget => false,
        };
        if !allowed {
            return Err(Error::State);
        }
        Ok(Self {
            phase: Phase::Stopped(stop),
            ..self
        })
    }

    pub fn checkpoint(self) -> [u8; 23] {
        let tag = match self.phase {
            Phase::Ready => 0,
            Phase::Preparing => 1,
            Phase::ModelWait => 2,
            Phase::ToolsWait => 3,
            Phase::FinalReview => 4,
            Phase::AfterTools => 5,
            Phase::Stopped(Stop::Complete) => 6,
            Phase::Stopped(Stop::Iterations) => 7,
            Phase::Stopped(Stop::Budget) => 8,
            Phase::Stopped(Stop::ToolPolicy) => 9,
            Phase::Stopped(Stop::ContentFilter) => 10,
            Phase::Stopped(Stop::RecoveryBudget) => 11,
            Phase::Stopped(Stop::Cancelled) => 12,
        };
        let mut bytes = [0; 23];
        bytes[..4].copy_from_slice(b"DTU1");
        bytes[4] = tag;
        bytes[5..9].copy_from_slice(&self.iterations.to_be_bytes());
        bytes[9..17].copy_from_slice(&self.request_ordinal.to_be_bytes());
        bytes[17..].copy_from_slice(&self.recovery.checkpoint());
        bytes
    }
    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 23 || &bytes[..4] != b"DTU1" {
            return Err(Error::Encoding);
        }
        let phase = match bytes[4] {
            0 => Phase::Ready,
            1 => Phase::Preparing,
            2 => Phase::ModelWait,
            3 => Phase::ToolsWait,
            4 => Phase::FinalReview,
            5 => Phase::AfterTools,
            6 => Phase::Stopped(Stop::Complete),
            7 => Phase::Stopped(Stop::Iterations),
            8 => Phase::Stopped(Stop::Budget),
            9 => Phase::Stopped(Stop::ToolPolicy),
            10 => Phase::Stopped(Stop::ContentFilter),
            11 => Phase::Stopped(Stop::RecoveryBudget),
            12 => Phase::Stopped(Stop::Cancelled),
            _ => return Err(Error::Encoding),
        };
        let iterations = u32::from_be_bytes(bytes[5..9].try_into().map_err(|_| Error::Encoding)?);
        let request_ordinal =
            u64::from_be_bytes(bytes[9..17].try_into().map_err(|_| Error::Encoding)?);
        let recovery = RecoveryState::restore(&bytes[17..]).map_err(|_| Error::Encoding)?;
        if (matches!(
            phase,
            Phase::Preparing
                | Phase::ModelWait
                | Phase::ToolsWait
                | Phase::FinalReview
                | Phase::AfterTools
                | Phase::Stopped(
                    Stop::Complete | Stop::ToolPolicy | Stop::ContentFilter | Stop::RecoveryBudget
                )
        ) && iterations == 0)
            || (matches!(
                phase,
                Phase::ModelWait
                    | Phase::ToolsWait
                    | Phase::FinalReview
                    | Phase::AfterTools
                    | Phase::Stopped(
                        Stop::Complete
                            | Stop::ToolPolicy
                            | Stop::ContentFilter
                            | Stop::RecoveryBudget
                    )
            ) && request_ordinal == 0)
        {
            return Err(Error::Encoding);
        }
        let terminal_recovery = recovery
            .advance(RecoveryObservation::Response {
                incomplete_reason: None,
                executable_calls: false,
            })
            .decision;
        match (phase, terminal_recovery) {
            (Phase::Stopped(Stop::ContentFilter), Decision::Halt(HaltReason::ContentFilter))
            | (Phase::Stopped(Stop::RecoveryBudget), Decision::Halt(HaltReason::RetryBudget)) => {}
            (Phase::Stopped(Stop::ContentFilter | Stop::RecoveryBudget), _)
            | (_, Decision::Halt(_)) => return Err(Error::Encoding),
            _ => {}
        }
        if matches!(
            phase,
            Phase::ToolsWait
                | Phase::FinalReview
                | Phase::AfterTools
                | Phase::Stopped(Stop::Complete | Stop::ToolPolicy)
        ) && recovery != RecoveryState::default()
        {
            return Err(Error::Encoding);
        }
        Ok(Self {
            phase,
            iterations,
            request_ordinal,
            recovery,
        })
    }
}
