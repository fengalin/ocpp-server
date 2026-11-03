use log::{error, info, warn};
use std::{
    fmt::{self, Write},
    ops,
};

// FIXME make those a setting also they are not BMS related
pub const DEFAULT_CAPACITY: u32 = 48_100;
pub const MAX_POWER: u32 = 7_400;
pub const STATIC_LOSS: u32 = 400;
pub const MAX_AVAILABLE_POWER: u32 = MAX_POWER - STATIC_LOSS;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Bms {
    pub capacity: u32,
    pub initial_energy: Option<u64>,
    pub current_energy: Option<u64>,
    pub initial_soc: SoC,
    pub current_soc: SoC,
    pub target_soc: Option<f32>,
}

impl Bms {
    pub fn builder(capacity: u32) -> BmsBuilder {
        BmsBuilder(Bms {
            capacity,
            initial_energy: None,
            current_energy: None,
            initial_soc: SoC::Unknown,
            current_soc: SoC::Unknown,
            target_soc: None,
        })
    }

    // FIXME could return a Result
    /// Record a new meter energy reading (Wh) and re-estimate `current_soc`
    /// from it (relative to `initial_energy`).
    pub fn update_from_energy(&mut self, energy: u64) {
        let Some(initial_energy) = self.initial_energy else {
            return;
        };

        if let Some(prev_energy) = self.current_energy {
            if energy == prev_energy {
                return;
            } else if prev_energy > energy {
                error!(
                    "can't compute SoC progress: new energy: {:.3} kWh, \
                    is less than previous: {:.3} kWh",
                    energy as f32 / 1_000.0,
                    prev_energy as f32 / 1_000.0,
                );
                return;
            }
        } else if initial_energy > energy {
            error!(
                "can't compute SoC progress: new energy: {:.3} kWh, \
                initial energy: {:.3} kWh",
                energy as f64 / 1_000.0,
                initial_energy as f64 / 1_000.0,
            );
            return;
        } else if energy == initial_energy {
            return;
        }

        let added_energy = energy - initial_energy;

        self.current_energy = Some(energy);
        self.current_soc = self.initial_soc + (added_energy as f32 / self.capacity as f32);
    }

    pub fn soc_progress(&self) -> SoCProgress {
        SoCProgress::from_soc_and_cap(self.current_soc, self.target_soc)
    }

    /// Computes the raw energy to add to reach the target SoC,
    /// not including constant power loss
    pub fn target_soc_to_energy(&self, target_soc: SoC) -> Option<u32> {
        use SoC::*;
        let soc_to_add = match (self.current_soc, target_soc) {
            (Absolute(cur_soc), Absolute(target_soc)) => saturating_sub(target_soc, cur_soc),
            (Relative(cur_soc), Relative(target_soc)) => saturating_sub(target_soc, cur_soc),
            _ => return None,
        };

        Some((self.capacity as f32 * soc_to_add) as u32)
    }

    /// The `target_soc` field, as a [`SoC`] of the same kind as `current_soc`
    /// (named `target` so it does not clash with the `target_soc` field).
    pub fn target(&self) -> SoC {
        use SoC::*;

        match self.current_soc {
            Absolute(_) => SoC::new_absolute(self.target_soc),
            Relative(_) => SoC::new_relative(self.target_soc),
            Unknown => SoC::new_relative(self.target_soc),
        }
    }
}

impl fmt::Display for Bms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "cap.: {:.3} kWh, init energy: {} kWh, \
            cur energy: {}, init SoC: {} %, cur SoC: {}",
            self.capacity as f32 / 1_000.0,
            self.initial_energy.map_or_else(
                || "N/A".to_string(),
                |e| format!("{:.3}", e as f64 / 1_000.0)
            ),
            self.current_energy.map_or_else(
                || "N/A".to_string(),
                |e| format!("{:.3}", e as f64 / 1_000.0)
            ),
            self.initial_soc,
            SoCProgress::from_soc_and_cap(self.current_soc, self.target_soc),
        ))
    }
}

pub struct BmsBuilder(Bms);
impl BmsBuilder {
    pub fn initial_energy(mut self, energy: u64) -> Self {
        self.0.initial_energy = Some(energy);
        self
    }
    pub fn current_energy(mut self, energy: u64) -> Self {
        self.0.current_energy = Some(energy);
        self
    }
    pub fn initial_soc(mut self, soc: SoC) -> Self {
        self.0.initial_soc = soc;
        self
    }
    pub fn current_soc(mut self, soc: SoC) -> Self {
        self.0.current_soc = soc;
        self
    }
    pub fn target_soc(mut self, target_soc: impl Into<Option<f32>>) -> Self {
        self.0.target_soc = target_soc.into();
        self
    }
    pub fn build(mut self) -> Bms {
        if self.0.current_soc.is_unknown() {
            self.0.current_soc = self.0.initial_soc;
        }
        self.0
    }
}

fn saturating_sub(lhs: f32, rhs: f32) -> f32 {
    if rhs >= lhs {
        return 0.0;
    }
    lhs - rhs
}

/// State of charge: a *fraction* 0–1 (see the crate docs). `PartialEq` is
/// exact (the old epsilon-0.01 impl was non-transitive and leaked into
/// `Bms`'s derived `PartialEq`).
#[derive(Debug, Default, Copy, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SoC {
    #[default]
    Unknown,
    Absolute(f32),
    Relative(f32),
}

impl SoC {
    pub fn new(is_absolute: bool, soc: impl Into<Option<f32>>) -> Self {
        if is_absolute {
            Self::new_absolute(soc)
        } else {
            Self::new_relative(soc)
        }
    }

    pub fn new_absolute(soc: impl Into<Option<f32>>) -> Self {
        match soc.into() {
            Some(soc) => SoC::Absolute(soc),
            None => SoC::Unknown,
        }
    }

    pub fn new_relative(soc: impl Into<Option<f32>>) -> Self {
        match soc.into() {
            Some(soc) => SoC::Relative(soc),
            None => SoC::Unknown,
        }
    }

    pub fn is_absolute(self) -> bool {
        matches!(self, SoC::Absolute(_))
    }

    pub fn is_relative(self) -> bool {
        matches!(self, SoC::Relative(_))
    }

    pub fn is_unknown(self) -> bool {
        matches!(self, SoC::Unknown)
    }

    pub fn absolute(self) -> Option<f32> {
        self.is_absolute().then(|| self.inner()).flatten()
    }

    pub fn relative(self) -> Option<f32> {
        self.is_relative().then(|| self.inner()).flatten()
    }

    /// The inner fraction, for either kind (`None` when `Unknown`).
    pub fn inner(self) -> Option<f32> {
        match self {
            SoC::Absolute(soc) => Some(soc),
            SoC::Relative(rel_soc) => Some(rel_soc),
            SoC::Unknown => None,
        }
    }

    // FIXME could return a status
    pub fn update(&mut self, new_soc: SoC) {
        use SoC::*;
        match self {
            Unknown => (),
            Absolute(_) => match new_soc {
                Unknown => {
                    warn!("forgetting absolute SoC {self}");
                }
                Absolute(_) => {
                    info!("Replacing absolute SoC {self} with {new_soc}");
                }
                Relative(_) => {
                    warn!("Replacing absolute SoC {self} with relative {new_soc}");
                }
            },
            Relative(_) => match new_soc {
                Unknown => {
                    warn!("forgetting relative SoC {self}");
                }
                Absolute(_) => {
                    info!("Replacing relative SoC {self} with absolute {new_soc}");
                }
                Relative(_) => {
                    warn!("Replacing relative SoC {self} with {new_soc}");
                }
            },
        }

        *self = new_soc;
    }
}

impl ops::Add<f32> for SoC {
    type Output = SoC;
    fn add(mut self, added_soc: f32) -> Self::Output {
        match &mut self {
            SoC::Absolute(soc) => *soc += added_soc,
            SoC::Relative(rel_soc) => *rel_soc += added_soc,
            SoC::Unknown => self = SoC::Relative(added_soc),
        }
        self
    }
}

impl ops::Sub<f32> for SoC {
    type Output = SoC;
    fn sub(mut self, soc_delta: f32) -> Self::Output {
        match &mut self {
            SoC::Absolute(soc) => *soc -= soc_delta,
            SoC::Relative(rel_soc) => *rel_soc -= soc_delta,
            SoC::Unknown => self = SoC::Relative(soc_delta),
        }
        self
    }
}

impl fmt::Display for SoC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn fmt_soc(f: &mut fmt::Formatter<'_>, soc: f32) -> fmt::Result {
            f.write_fmt(format_args!("{:.1} %", soc * 100.0))
        }
        match self {
            SoC::Absolute(soc) => fmt_soc(f, *soc),
            SoC::Relative(soc) => {
                f.write_char('+')?;
                fmt_soc(f, *soc)
            }
            SoC::Unknown => f.write_str("unknown"),
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum SoCProgress {
    Unknown,
    AbsoluteUncapped(f32),
    AbsoluteCap(f32),
    AbsoluteTargetNotReached { soc: f32, target: f32 },
    AbsoluteTargetReached { soc: f32, target: f32 },
    RelativeUncapped(f32),
    RelativeCap(f32),
    RelativeTargetNotReached { rel_soc: f32, target: f32 },
    RelativeCapReached { rel_soc: f32, cap: f32 },
}

impl SoCProgress {
    pub fn new(
        is_absolute: bool,
        soc: impl Into<Option<f32>>,
        cap: impl Into<Option<f32>>,
    ) -> Self {
        let soc = soc.into().map(|soc| soc.clamp(0.0, 1.0));
        let cap = cap.into().map(|cap| cap.clamp(0.0, 1.0));

        use SoCProgress::*;
        if is_absolute {
            match (soc, cap) {
                (Some(soc), Some(cap)) if soc < cap => {
                    AbsoluteTargetNotReached { soc, target: cap }
                }
                (Some(soc), Some(cap)) => AbsoluteTargetReached { soc, target: cap },
                (Some(soc), None) => AbsoluteUncapped(soc),
                (None, Some(cap)) => AbsoluteCap(cap),
                (None, None) => Unknown,
            }
        } else {
            match (soc, cap) {
                (Some(rel_soc), Some(cap)) if rel_soc < cap => RelativeTargetNotReached {
                    rel_soc,
                    target: cap,
                },
                (Some(rel_soc), Some(cap)) => RelativeCapReached { rel_soc, cap },
                (Some(rel_soc), None) => RelativeUncapped(rel_soc),
                (None, Some(cap)) => RelativeCap(cap),
                (None, None) => Unknown,
            }
        }
    }

    pub fn from_soc_and_cap(soc: SoC, cap: impl Into<Option<f32>>) -> Self {
        Self::new(soc.is_absolute(), soc.inner(), cap)
    }

    pub fn is_complete(self) -> bool {
        use SoCProgress::*;
        matches!(
            self,
            AbsoluteTargetReached { .. } | RelativeCapReached { .. }
        )
    }

    pub fn soc(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(soc) => SoC::Absolute(soc),
            AbsoluteCap(_) => SoC::Unknown,
            AbsoluteTargetNotReached { soc, .. } => SoC::Absolute(soc),
            AbsoluteTargetReached { soc, .. } => SoC::Absolute(soc),
            RelativeUncapped(rel_soc) => SoC::Relative(rel_soc),
            RelativeCap(_) => SoC::Unknown,
            RelativeTargetNotReached { rel_soc, .. } => SoC::Relative(rel_soc),
            RelativeCapReached { rel_soc, .. } => SoC::Relative(rel_soc),
        }
    }

    pub fn cap(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(_) => SoC::Unknown,
            AbsoluteCap(cap) => SoC::Absolute(cap),
            AbsoluteTargetNotReached { target: cap, .. } => SoC::Absolute(cap),
            AbsoluteTargetReached { target: cap, .. } => SoC::Absolute(cap),
            RelativeUncapped(_) => SoC::Unknown,
            RelativeCap(cap) => SoC::Relative(cap),
            RelativeTargetNotReached { target: cap, .. } => SoC::Relative(cap),
            RelativeCapReached { cap, .. } => SoC::Relative(cap),
        }
    }

    pub fn absolute_soc(self) -> Option<f32> {
        self.soc().absolute()
    }
}

impl fmt::Display for SoCProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SoCProgress::*;
        match self {
            Unknown => f.write_str("unknown"),
            AbsoluteUncapped(soc) => write!(f, "uncapped {}", SoC::Absolute(*soc)),
            RelativeUncapped(added_soc) => write!(f, "uncapped {}", SoC::Relative(*added_soc)),
            RelativeCap(cap) => write!(f, "capped to {}", SoC::Relative(*cap)),
            _ => {
                write!(f, "{} / {}", self.soc(), self.cap())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soc_progress_constructor() {
        use SoCProgress::*;
        assert_eq!(SoCProgress::new(true, None, None), Unknown);
        assert_eq!(
            SoCProgress::new(true, Some(0.42), None),
            AbsoluteUncapped(0.42)
        );
        assert_eq!(SoCProgress::new(true, None, Some(0.50)), AbsoluteCap(0.50));
        assert_eq!(
            SoCProgress::new(true, Some(0.42), Some(0.50)),
            AbsoluteTargetNotReached {
                soc: 0.42,
                target: 0.50
            }
        );
        assert_eq!(
            SoCProgress::new(true, Some(0.50), Some(0.50)),
            AbsoluteTargetReached {
                soc: 0.50,
                target: 0.50
            }
        );

        assert_eq!(SoCProgress::new(false, None, None), Unknown);
        assert_eq!(
            SoCProgress::new(false, Some(0.42), None),
            RelativeUncapped(0.42)
        );
        assert_eq!(SoCProgress::new(false, None, Some(0.50)), RelativeCap(0.50));
        assert_eq!(
            SoCProgress::new(false, Some(0.42), Some(0.50)),
            RelativeTargetNotReached {
                rel_soc: 0.42,
                target: 0.50
            }
        );
        assert_eq!(
            SoCProgress::new(false, Some(0.50), Some(0.50)),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50
            }
        );
    }
}
