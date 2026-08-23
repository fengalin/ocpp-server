use log::{error, info, warn};
use std::{
    cmp,
    fmt::{self, Write},
    ops,
};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Bms {
    pub capacity: f64,
    pub constant_power_loss: u16,
    pub initial_energy: Option<f64>,
    pub current_energy: Option<f64>,
    pub initial_soc: SoC,
    pub current_soc: SoC,
    pub soc_cap: Option<f64>,
}

impl Bms {
    pub fn builder(capacity: u32, constant_power_loss: u16) -> BmsBuilder {
        BmsBuilder(Bms {
            capacity: capacity as f64,
            constant_power_loss,
            initial_energy: None,
            current_energy: None,
            initial_soc: SoC::Unknown,
            current_soc: SoC::Unknown,
            soc_cap: None,
        })
    }

    // FIXME could return a Result
    pub fn have_energy(&mut self, energy: u64) {
        let Some(initial_energy) = self.initial_energy else {
            return;
        };
        let energy = energy as f64;

        if let Some(prev_energy) = self.current_energy {
            if prev_energy > energy {
                error!(
                    "can't compute SoC progress: new energy: {:.3} kWh, \
                    is greater than previous: {:.3} kWh",
                    energy / 1_000.0,
                    prev_energy / 1_000.0,
                );
                return;
            } else if (energy - prev_energy).abs() < 1.0 {
                return;
            }
        } else if initial_energy > energy {
            error!(
                "can't compute SoC progress: new energy: {:.3} kWh, \
                initial energy: {:.3} kWh",
                energy / 1_000.0,
                initial_energy / 1_000.0,
            );
            return;
        } else if (energy - initial_energy).abs() < 1.0 {
            return;
        }

        let added_energy = energy - initial_energy;

        self.current_energy = Some(energy);
        self.current_soc = self.initial_soc + (added_energy / self.capacity);
    }

    pub fn soc_progress(&self) -> SoCProgress {
        SoCProgress::from_soc_and_cap(self.current_soc, self.soc_cap)
    }

    /// Computes the raw energy to add to reach the target SoC,
    /// not including constant power loss
    pub fn energy_to_soc(&self, target_soc: SoC) -> Option<f64> {
        use SoC::*;
        let soc_to_add = match (self.current_soc, target_soc) {
            (Absolute(cur_soc), Absolute(target_soc)) => saturating_sub(target_soc, cur_soc),
            (Relative(cur_soc), Relative(target_soc)) => saturating_sub(target_soc, cur_soc),
            _ => return None,
        };

        Some(self.capacity * soc_to_add)
    }

    pub fn soc_cap(&self) -> SoC {
        use SoC::*;

        match self.current_soc {
            Absolute(_) => SoC::new_absolute(self.soc_cap),
            Relative(_) => SoC::new_relative(self.soc_cap),
            Unknown => SoC::new_relative(self.soc_cap),
        }
    }
}

pub struct BmsBuilder(Bms);
impl BmsBuilder {
    pub fn initial_energy(mut self, energy: u64) -> Self {
        self.0.initial_energy = Some(energy as f64);
        self
    }
    pub fn current_energy(mut self, energy: u64) -> Self {
        self.0.current_energy = Some(energy as f64);
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
    pub fn soc_cap(mut self, soc_cap: impl Into<Option<f64>>) -> Self {
        self.0.soc_cap = soc_cap.into();
        self
    }
    pub fn build(mut self) -> Bms {
        if self.0.current_soc.is_unknown() {
            self.0.current_soc = self.0.initial_soc;
        }
        self.0
    }
}

fn saturating_sub(lhs: f64, rhs: f64) -> f64 {
    if rhs >= lhs {
        return 0.0;
    }
    lhs - rhs
}

#[derive(Debug, Default, Copy, Clone, serde::Deserialize, serde::Serialize)]
pub enum SoC {
    #[default]
    Unknown,
    Absolute(f64),
    Relative(f64),
}

impl SoC {
    pub fn new(is_absolute: bool, soc: impl Into<Option<f64>>) -> Self {
        match soc.into() {
            Some(soc) if is_absolute => SoC::Absolute(soc),
            Some(soc) => SoC::Relative(soc),
            None => SoC::Unknown,
        }
    }

    pub fn new_absolute(soc: impl Into<Option<f64>>) -> Self {
        match soc.into() {
            Some(soc) => SoC::Absolute(soc),
            None => SoC::Unknown,
        }
    }

    pub fn new_relative(soc: impl Into<Option<f64>>) -> Self {
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

    pub fn absolute(self) -> Option<f64> {
        let SoC::Absolute(soc) = self else {
            return None;
        };

        Some(soc)
    }

    pub fn relative(self) -> Option<f64> {
        let SoC::Relative(soc) = self else {
            return None;
        };

        Some(soc)
    }

    pub fn inner(self) -> Option<f64> {
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

impl ops::Add<f64> for SoC {
    type Output = SoC;
    fn add(mut self, added_soc: f64) -> Self::Output {
        match &mut self {
            SoC::Absolute(soc) => *soc += added_soc,
            SoC::Relative(rel_soc) => *rel_soc += added_soc,
            SoC::Unknown => self = SoC::Relative(added_soc),
        }
        self
    }
}

impl ops::Sub<f64> for SoC {
    type Output = SoC;
    fn sub(mut self, soc_delta: f64) -> Self::Output {
        match &mut self {
            SoC::Absolute(soc) => *soc -= soc_delta,
            SoC::Relative(rel_soc) => *rel_soc -= soc_delta,
            SoC::Unknown => self = SoC::Relative(soc_delta),
        }
        self
    }
}

impl cmp::PartialEq for SoC {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SoC::Absolute(this), SoC::Absolute(other)) => (this - other).abs() < 0.01,
            (SoC::Relative(this), SoC::Relative(other)) => (this - other).abs() < 0.01,
            (SoC::Unknown, SoC::Unknown) => true,
            _ => false,
        }
    }
}

impl fmt::Display for SoC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn fmt_soc(f: &mut fmt::Formatter<'_>, soc: f64) -> fmt::Result {
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
    AbsoluteUncapped(f64),
    AbsoluteCap(f64),
    AbsoluteCapNotReached { soc: f64, cap: f64 },
    AbsoluteCapReached { soc: f64, cap: f64 },
    RelativeUncapped(f64),
    RelativeCap(f64),
    RelativeCapNotReached { rel_soc: f64, cap: f64 },
    RelativeCapReached { rel_soc: f64, cap: f64 },
}

impl SoCProgress {
    pub fn new(
        is_absolute: bool,
        soc: impl Into<Option<f64>>,
        cap: impl Into<Option<f64>>,
    ) -> Self {
        let soc = soc.into().map(|soc| soc.clamp(0.0, 1.0));
        let cap = cap.into().map(|cap| cap.clamp(0.0, 1.0));

        use SoCProgress::*;
        if is_absolute {
            match (soc, cap) {
                (Some(soc), Some(cap)) if soc < cap => AbsoluteCapNotReached { soc, cap },
                (Some(soc), Some(cap)) => AbsoluteCapReached { soc, cap },
                (Some(soc), None) => AbsoluteUncapped(soc),
                (None, Some(cap)) => AbsoluteCap(cap),
                (None, None) => Unknown,
            }
        } else {
            match (soc, cap) {
                (Some(rel_soc), Some(cap)) if rel_soc < cap => {
                    RelativeCapNotReached { rel_soc, cap }
                }
                (Some(rel_soc), Some(cap)) => RelativeCapReached { rel_soc, cap },
                (Some(rel_soc), None) => RelativeUncapped(rel_soc),
                (None, Some(cap)) => RelativeCap(cap),
                (None, None) => Unknown,
            }
        }
    }

    pub fn from_soc_and_cap(soc: SoC, cap: impl Into<Option<f64>>) -> Self {
        Self::new(soc.is_absolute(), soc.inner(), cap)
    }

    pub fn is_complete(self) -> bool {
        use SoCProgress::*;
        matches!(self, AbsoluteCapReached { .. } | RelativeCapReached { .. })
    }

    pub fn soc(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(soc) => SoC::Absolute(soc),
            AbsoluteCap(_) => SoC::Unknown,
            AbsoluteCapNotReached { soc, .. } => SoC::Absolute(soc),
            AbsoluteCapReached { soc, .. } => SoC::Absolute(soc),
            RelativeUncapped(rel_soc) => SoC::Relative(rel_soc),
            RelativeCap(_) => SoC::Unknown,
            RelativeCapNotReached { rel_soc, .. } => SoC::Relative(rel_soc),
            RelativeCapReached { rel_soc, .. } => SoC::Relative(rel_soc),
        }
    }

    pub fn cap(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(_) => SoC::Unknown,
            AbsoluteCap(cap) => SoC::Absolute(cap),
            AbsoluteCapNotReached { cap, .. } => SoC::Absolute(cap),
            AbsoluteCapReached { cap, .. } => SoC::Absolute(cap),
            RelativeUncapped(_) => SoC::Unknown,
            RelativeCap(cap) => SoC::Relative(cap),
            RelativeCapNotReached { cap, .. } => SoC::Relative(cap),
            RelativeCapReached { cap, .. } => SoC::Relative(cap),
        }
    }

    pub fn absolute_soc(self) -> Option<f64> {
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
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50
            }
        );
        assert_eq!(
            SoCProgress::new(true, Some(0.50), Some(0.50)),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50
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
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50
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
