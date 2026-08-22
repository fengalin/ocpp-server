use log::{error, warn};
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
    pub initial_soc_and_cap: SoCProgress,
}

impl Bms {
    pub fn new(
        capacity: u32,
        constant_power_loss: u16,
        initial_energy: impl Into<Option<u64>>,
        initial_soc: SoC,
        cap: impl Into<Option<f64>>,
    ) -> Self {
        let cap = cap.into();

        let initial_soc_and_cap = match (initial_soc, cap) {
            (SoC::Absolute(mut soc), Some(mut cap)) => {
                check_fix_range(&mut soc);
                check_fix_range(&mut cap);
                if soc >= cap {
                    SoCProgress::AbsoluteCapReached { soc, cap }
                } else {
                    SoCProgress::AbsoluteCapNotReached { soc, cap }
                }
            }
            (SoC::Relative(mut rel_soc), Some(mut cap)) => {
                check_fix_range(&mut rel_soc);
                check_fix_range(&mut cap);
                if rel_soc >= cap {
                    SoCProgress::RelativeCapReached { rel_soc, cap }
                } else {
                    SoCProgress::RelativeCapNotReached { rel_soc, cap }
                }
            }
            (SoC::Absolute(mut soc), None) => {
                check_fix_range(&mut soc);
                SoCProgress::AbsoluteUncapped(soc)
            }
            (SoC::Relative(mut rel_soc), None) => {
                check_fix_range(&mut rel_soc);
                SoCProgress::RelativeUncapped(rel_soc)
            }
            (SoC::Unknown, Some(mut cap)) => {
                check_fix_range(&mut cap);
                SoCProgress::RelativeCap(cap)
            }
            (SoC::Unknown, None) => SoCProgress::Unknown,
        };

        Bms {
            capacity: capacity as f64,
            constant_power_loss,
            initial_energy: initial_energy.into().map(|e| e as f64),
            initial_soc_and_cap,
        }
    }

    pub fn get_current_soc(&self, energy: u64) -> SoCProgress {
        let Some(initial_energy) = self.initial_energy else {
            return SoCProgress::Unknown;
        };
        let added_energy = energy as f64 - initial_energy;
        if added_energy < 0.0 {
            log::error!(
                "can't compute SoC progress: new energy: {energy}, reference energy: {initial_energy:.0}"
            );
            return SoCProgress::Unknown;
        };

        if added_energy < 1.0 {
            return self.initial_soc_and_cap;
        }

        self.initial_soc_and_cap
            .saturating_add(added_energy / self.capacity)
    }

    /// Computes the raw energy to add, not including constant power loss.
    pub fn get_energy_to_add(&self) -> Option<f64> {
        let soc_to_add = match (
            self.initial_soc_and_cap.soc(),
            self.initial_soc_and_cap.cap(),
        ) {
            (SoC::Absolute(soc), SoC::Absolute(cap)) | (SoC::Relative(soc), SoC::Relative(cap)) => {
                saturating_sub(cap, soc)
            }
            (SoC::Unknown, _) => return None,
            _ => {
                error!("FIXME: invalid BMS SoC & cap combination: {self:?}");
                return None;
            }
        };

        Some(self.capacity * soc_to_add)
    }
}

fn check_fix_range(s: &mut f64) {
    *s = (*s).clamp(0.0, 1.0);
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
}

impl ops::Add<f64> for SoC {
    type Output = SoC;
    fn add(mut self, added_soc: f64) -> Self::Output {
        let saturating_add_assign = |s: &mut f64, added_soc: f64| {
            *s += added_soc;
            if *s > 1.0 {
                *s = 1.0;
            }
        };
        match &mut self {
            SoC::Absolute(soc) => saturating_add_assign(soc, added_soc),
            SoC::Relative(rel_soc) => saturating_add_assign(rel_soc, added_soc),
            SoC::Unknown => {
                let mut rel_soc = 0.0;
                saturating_add_assign(&mut rel_soc, added_soc);
                self = SoC::Relative(rel_soc);
            }
        }

        self
    }
}

impl cmp::PartialEq for SoC {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SoC::Absolute(this), SoC::Absolute(other)) => (this - other).abs() < 0.009,
            (SoC::Relative(this), SoC::Relative(other)) => (this - other).abs() < 0.009,
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
        let soc = soc.into();
        let cap = cap.into();

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

    pub fn is_complete(self) -> bool {
        use SoCProgress::*;
        matches!(self, AbsoluteCapReached { .. } | RelativeCapReached { .. })
    }

    pub fn is_absolute(self) -> bool {
        use SoCProgress::*;
        matches!(
            self,
            AbsoluteCapReached { .. } | AbsoluteCapNotReached { .. } | AbsoluteUncapped(_)
        )
    }

    pub fn update_soc(&mut self, new_soc: SoC) {
        use SoCProgress::*;
        match self {
            Unknown => match new_soc {
                SoC::Absolute(new_abs_soc) => *self = AbsoluteUncapped(new_abs_soc),
                SoC::Relative(new_rel_soc) => *self = RelativeUncapped(new_rel_soc),
                SoC::Unknown => (),
            },
            AbsoluteUncapped(soc) => match new_soc {
                SoC::Absolute(new_abs_soc) => *soc = new_abs_soc,
                SoC::Relative(new_rel_soc) => {
                    warn!(
                        "replacing absolute SoC in {self} with {new_soc} as per user instructions",
                    );
                    *self = RelativeUncapped(new_rel_soc);
                }
                SoC::Unknown => {
                    warn!("forgetting absolute SoC in {self} as per user instructions");
                    *self = Unknown;
                }
            },
            AbsoluteCap(cap) => match new_soc {
                SoC::Absolute(new_abs_soc) => *self = Self::new(true, new_abs_soc, *cap),
                SoC::Relative(new_rel_soc) => {
                    let cap = *cap;
                    warn!(
                        "associating absolute SoC cap in {self} with {new_soc} as per user instructions",
                    );
                    *self = Self::new(false, new_rel_soc, cap);
                }
                SoC::Unknown => (),
            },
            AbsoluteCapNotReached { cap, .. } | AbsoluteCapReached { cap, .. } => match new_soc {
                SoC::Absolute(new_abs_soc) => *self = Self::new(true, new_abs_soc, *cap),
                SoC::Relative(new_rel_soc) => {
                    let cap = *cap;
                    warn!(
                        "replacing absolute SoC in {self} with {new_soc} as per user instructions",
                    );
                    *self = Self::new(false, new_rel_soc, cap);
                }
                SoC::Unknown => {
                    let cap = *cap;
                    warn!("forgetting absolute SoC in {self} as per user instructions",);
                    *self = AbsoluteCap(cap);
                }
            },
            RelativeUncapped(rel_soc) => match new_soc {
                SoC::Absolute(new_abs_soc) => {
                    warn!(
                        "replacing relative SoC in {self} with {new_soc} as per user instructions",
                    );
                    *self = AbsoluteUncapped(new_abs_soc);
                }
                SoC::Relative(new_rel_soc) => *rel_soc = new_rel_soc,
                SoC::Unknown => *self = Unknown,
            },
            RelativeCap(cap) => match new_soc {
                SoC::Relative(new_rel_soc) => *self = Self::new(false, new_rel_soc, *cap),
                SoC::Absolute(new_abs_soc) => {
                    let cap = *cap;
                    warn!(
                        "associating relative SoC cap in {self} with {new_soc} as per user instructions",
                    );
                    *self = Self::new(true, new_abs_soc, cap);
                }
                SoC::Unknown => (),
            },
            RelativeCapNotReached { cap, .. } | RelativeCapReached { cap, .. } => match new_soc {
                SoC::Relative(new_rel_soc) => *self = Self::new(false, new_rel_soc, *cap),
                SoC::Absolute(new_abs_soc) => {
                    let cap = *cap;
                    warn!(
                        "replacing relative SoC in {self} with {new_soc} as per user instructions",
                    );
                    *self = Self::new(true, new_abs_soc, cap);
                }
                SoC::Unknown => {
                    let cap = *cap;
                    warn!("forgetting relative SoC in {self} as per user instructions");
                    *self = RelativeCap(cap);
                }
            },
        }
    }

    pub fn update_cap(&mut self, new_cap: SoC) {
        use SoCProgress::*;
        match self {
            Unknown => match new_cap {
                SoC::Absolute(new_cap) => *self = AbsoluteCap(new_cap),
                SoC::Relative(new_cap) => *self = RelativeCap(new_cap),
                SoC::Unknown => (),
            },
            AbsoluteUncapped(soc) => match new_cap {
                SoC::Absolute(new_cap) => *self = Self::new(true, *soc, new_cap),
                SoC::Relative(new_cap) => {
                    let soc = *soc;
                    warn!(
                        "associating absolute SoC in {self} with cap {new_cap} as per user instructions",
                    );
                    *self = Self::new(false, soc, new_cap);
                }
                SoC::Unknown => (),
            },
            AbsoluteCap(_) => match new_cap {
                SoC::Absolute(new_abs_cap) => *self = AbsoluteCap(new_abs_cap),
                SoC::Relative(new_rel_cap) => {
                    warn!(
                        "replacing absolute uncapped SoC cap in {self} with cap {new_cap} as per user instructions",
                    );
                    *self = RelativeCap(new_rel_cap);
                }
                SoC::Unknown => {
                    warn!("forgetting {self} as per user instructions");
                    *self = Unknown;
                }
            },
            AbsoluteCapNotReached { soc, .. } | AbsoluteCapReached { soc, .. } => match new_cap {
                SoC::Absolute(new_cap) => *self = Self::new(true, *soc, new_cap),
                SoC::Relative(new_cap) => {
                    let soc = *soc;
                    warn!(
                        "replacing absolute SoC cap in {self} with cap {new_cap} as per user instructions",
                    );
                    *self = Self::new(true, soc, new_cap);
                }
                SoC::Unknown => {
                    let soc = *soc;
                    warn!("forgetting absolute SoC cap in {self} as per user instructions",);
                    *self = AbsoluteUncapped(soc);
                }
            },
            RelativeUncapped(rel_soc) => match new_cap {
                SoC::Relative(new_rel_cap) => *self = Self::new(false, *rel_soc, new_rel_cap),
                SoC::Absolute(new_abs_cap) => {
                    let rel_soc = *rel_soc;
                    warn!(
                        "associating relative uncapped SoC {self} with {new_cap} as per user instructions",
                    );
                    *self = Self::new(false, rel_soc, new_abs_cap);
                }
                SoC::Unknown => (),
            },
            RelativeCap(_) => match new_cap {
                SoC::Relative(new_rel_cap) => *self = RelativeCap(new_rel_cap),
                SoC::Absolute(new_abs_cap) => {
                    warn!(
                        "replacing relative uncapped SoC cap in {self} with cap {new_cap} as per user instructions",
                    );
                    *self = AbsoluteCap(new_abs_cap);
                }
                SoC::Unknown => {
                    warn!("forgetting {self} as per user instructions");
                    *self = Unknown;
                }
            },
            RelativeCapNotReached { rel_soc, .. } | RelativeCapReached { rel_soc, .. } => {
                match new_cap {
                    SoC::Relative(new_rel_cap) => *self = Self::new(false, *rel_soc, new_rel_cap),
                    SoC::Absolute(new_abs_cap) => {
                        let rel_soc = *rel_soc;
                        warn!(
                            "replacing relative SoC cap in {self} with cap {new_cap} as per user instructions",
                        );
                        *self = Self::new(false, rel_soc, new_abs_cap);
                    }
                    SoC::Unknown => {
                        let rel_soc = *rel_soc;
                        warn!("forgetting relative SoC cap in {self} as per user instructions");
                        *self = RelativeUncapped(rel_soc);
                    }
                }
            }
        }
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

    pub fn saturating_add(mut self, added_soc: f64) -> SoCProgress {
        let saturating_add_assign = |s: &mut f64, added_soc: f64| {
            *s += added_soc;
            if *s > 1.0 {
                *s = 1.0;
            }
        };
        match &mut self {
            SoCProgress::AbsoluteUncapped(soc) => saturating_add_assign(soc, added_soc),
            // FIXME this is problematic: the cap is expressed as an absolute cap
            // but all we know is the added SoC, not the initial SoC
            // yet, we need to be able to represent an AbsoluteCap,
            // e.g. as an intermediate state.
            SoCProgress::AbsoluteCap(cap) => {
                self = Self::new(false, added_soc.clamp(0.0, 1.0), *cap)
            }
            SoCProgress::AbsoluteCapNotReached { soc, cap } => {
                saturating_add_assign(soc, added_soc);
                self = Self::new(true, *soc, *cap);
            }
            SoCProgress::AbsoluteCapReached { soc, .. } => saturating_add_assign(soc, added_soc),
            SoCProgress::RelativeUncapped(rel_soc) => saturating_add_assign(rel_soc, added_soc),
            SoCProgress::RelativeCap(cap) => {
                self = Self::new(false, added_soc.clamp(0.0, 1.0), *cap)
            }
            SoCProgress::RelativeCapNotReached { rel_soc, cap } => {
                saturating_add_assign(rel_soc, added_soc);
                self = Self::new(false, *rel_soc, *cap);
            }
            SoCProgress::RelativeCapReached { rel_soc, .. } => {
                saturating_add_assign(rel_soc, added_soc)
            }
            SoCProgress::Unknown => self = SoCProgress::RelativeUncapped(added_soc.clamp(0.0, 1.0)),
        }

        self
    }

    pub fn saturating_sub(mut self, rhs: f64) -> SoCProgress {
        let saturating_sub_assign = |s: &mut f64, rhs: f64| {
            *s -= rhs;
            if *s < 0.0 {
                *s = 0.0;
            }
        };
        match &mut self {
            SoCProgress::AbsoluteUncapped(soc) => saturating_sub_assign(soc, rhs),
            SoCProgress::AbsoluteCap(_) => (),
            SoCProgress::AbsoluteCapNotReached { soc, cap } => {
                saturating_sub_assign(soc, rhs);
                self = Self::new(true, *soc, *cap);
            }
            SoCProgress::AbsoluteCapReached { soc, cap } => {
                saturating_sub_assign(soc, rhs);
                self = Self::new(true, *soc, *cap);
            }
            SoCProgress::RelativeUncapped(rel_soc) => saturating_sub_assign(rel_soc, rhs),
            SoCProgress::RelativeCapNotReached { rel_soc, cap } => {
                saturating_sub_assign(rel_soc, rhs);
                self = Self::new(false, *rel_soc, *cap);
            }
            SoCProgress::RelativeCapReached { rel_soc, cap } => {
                saturating_sub_assign(rel_soc, rhs);
                self = Self::new(false, *rel_soc, *cap);
            }
            SoCProgress::RelativeCap(_) => (),
            SoCProgress::Unknown => (),
        }

        self
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

    #[test]
    fn soc_progress_update_soc() {
        #[track_caller]
        fn test(init: SoCProgress, new_soc: SoC, res: SoCProgress) {
            let mut sp = init;
            sp.update_soc(new_soc);
            assert_eq!(sp, res);
        }

        use SoCProgress::*;

        // init Unknown

        test(Unknown, SoC::Unknown, Unknown);
        test(Unknown, SoC::Absolute(0.42), AbsoluteUncapped(0.42));
        test(Unknown, SoC::Relative(0.42), RelativeUncapped(0.42));

        // init AbsoluteUncapped

        test(AbsoluteUncapped(0.50), SoC::Unknown, Unknown);
        test(
            AbsoluteUncapped(0.50),
            SoC::Absolute(0.42),
            AbsoluteUncapped(0.42),
        );
        test(
            AbsoluteUncapped(0.50),
            SoC::Relative(0.42),
            RelativeUncapped(0.42),
        );

        // init AbsoluteCap

        test(AbsoluteCap(0.50), SoC::Unknown, AbsoluteCap(0.50));
        test(
            AbsoluteCap(0.50),
            SoC::Absolute(0.42),
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCap(0.50),
            SoC::Relative(0.42),
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
        );

        test(AbsoluteCap(0.50), SoC::Unknown, AbsoluteCap(0.50));
        test(
            AbsoluteCap(0.50),
            SoC::Absolute(0.50),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCap(0.50),
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );

        // init AbsoluteCapNotReached

        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Unknown,
            AbsoluteCap(0.50),
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.30),
            AbsoluteCapNotReached {
                soc: 0.30,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.30),
            RelativeCapNotReached {
                rel_soc: 0.30,
                cap: 0.50,
            },
        );

        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.50),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );

        // init AbsoluteCapReached

        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Unknown,
            AbsoluteCap(0.50),
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.30),
            AbsoluteCapNotReached {
                soc: 0.30,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.30),
            RelativeCapNotReached {
                rel_soc: 0.30,
                cap: 0.50,
            },
        );

        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.51),
            AbsoluteCapReached {
                soc: 0.51,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.51),
            RelativeCapReached {
                rel_soc: 0.51,
                cap: 0.50,
            },
        );

        // init RelativeUncapped

        test(RelativeUncapped(0.50), SoC::Unknown, Unknown);
        test(
            RelativeUncapped(0.50),
            SoC::Absolute(0.42),
            AbsoluteUncapped(0.42),
        );
        test(
            RelativeUncapped(0.50),
            SoC::Relative(0.42),
            RelativeUncapped(0.42),
        );

        // init RelativeCap

        test(RelativeCap(0.50), SoC::Unknown, RelativeCap(0.50));
        test(
            RelativeCap(0.50),
            SoC::Absolute(0.42),
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
        );
        test(
            RelativeCap(0.50),
            SoC::Relative(0.42),
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
        );

        test(RelativeCap(0.50), SoC::Unknown, RelativeCap(0.50));
        test(
            RelativeCap(0.50),
            SoC::Absolute(0.50),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeCap(0.50),
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );

        // init RelativeCapNotReached

        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Unknown,
            RelativeCap(0.50),
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.30),
            AbsoluteCapNotReached {
                soc: 0.30,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.30),
            RelativeCapNotReached {
                rel_soc: 0.30,
                cap: 0.50,
            },
        );

        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.50),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );

        // init RelativeCapReached

        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Unknown,
            RelativeCap(0.50),
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.30),
            AbsoluteCapNotReached {
                soc: 0.30,
                cap: 0.50,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.30),
            RelativeCapNotReached {
                rel_soc: 0.30,
                cap: 0.50,
            },
        );

        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.51),
            AbsoluteCapReached {
                soc: 0.51,
                cap: 0.50,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.51),
            RelativeCapReached {
                rel_soc: 0.51,
                cap: 0.50,
            },
        );
    }

    #[test]
    fn soc_progress_update_soc_cap() {
        #[track_caller]
        fn test(init: SoCProgress, new_cap: SoC, res: SoCProgress) {
            let mut sp = init;
            sp.update_cap(new_cap);
            assert_eq!(sp, res);
        }

        use SoCProgress::*;

        // init Unknown

        test(Unknown, SoC::Unknown, Unknown);
        test(Unknown, SoC::Absolute(0.42), AbsoluteCap(0.42));
        test(Unknown, SoC::Relative(0.42), RelativeCap(0.42));

        // init AbsoluteUncapped

        test(AbsoluteUncapped(0.50), SoC::Unknown, AbsoluteUncapped(0.50));
        test(
            AbsoluteUncapped(0.50),
            SoC::Absolute(0.50),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteUncapped(0.50),
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteUncapped(0.50),
            SoC::Absolute(0.60),
            AbsoluteCapNotReached {
                soc: 0.50,
                cap: 0.60,
            },
        );
        test(
            AbsoluteUncapped(0.50),
            SoC::Relative(0.60),
            RelativeCapNotReached {
                rel_soc: 0.50,
                cap: 0.60,
            },
        );

        // init AbsoluteCap

        test(AbsoluteCap(0.50), SoC::Unknown, Unknown);
        test(AbsoluteCap(0.50), SoC::Absolute(0.42), AbsoluteCap(0.42));
        test(AbsoluteCap(0.50), SoC::Relative(0.42), RelativeCap(0.42));

        // init AbsoluteCapNotReached

        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Unknown,
            AbsoluteUncapped(0.42),
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.60),
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.60,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.60),
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.60,
            },
        );

        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.40),
            AbsoluteCapReached {
                soc: 0.42,
                cap: 0.40,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.40),
            AbsoluteCapReached {
                soc: 0.42,
                cap: 0.40,
            },
        );

        // init AbsoluteCapReached

        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Unknown,
            AbsoluteUncapped(0.50),
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.60),
            AbsoluteCapNotReached {
                soc: 0.50,
                cap: 0.60,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.60),
            AbsoluteCapNotReached {
                soc: 0.50,
                cap: 0.60,
            },
        );

        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.45),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.45,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.45),
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.45,
            },
        );

        // init RelativeUncapped

        test(RelativeUncapped(0.50), SoC::Unknown, RelativeUncapped(0.50));
        test(
            RelativeUncapped(0.50),
            SoC::Absolute(0.60),
            RelativeCapNotReached {
                rel_soc: 0.50,
                cap: 0.60,
            },
        );
        test(
            RelativeUncapped(0.50),
            SoC::Relative(0.60),
            RelativeCapNotReached {
                rel_soc: 0.50,
                cap: 0.60,
            },
        );
        test(
            RelativeUncapped(0.50),
            SoC::Absolute(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeUncapped(0.50),
            SoC::Relative(0.50),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );

        // init RelativeCap

        test(RelativeCap(0.50), SoC::Unknown, Unknown);
        test(RelativeCap(0.50), SoC::Absolute(0.42), AbsoluteCap(0.42));
        test(RelativeCap(0.50), SoC::Relative(0.42), RelativeCap(0.42));

        // init RelativeCapNotReached

        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Unknown,
            RelativeUncapped(0.42),
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.45),
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.45,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.45),
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.45,
            },
        );

        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Absolute(0.40),
            RelativeCapReached {
                rel_soc: 0.42,
                cap: 0.40,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            SoC::Relative(0.40),
            RelativeCapReached {
                rel_soc: 0.42,
                cap: 0.40,
            },
        );

        // init RelativeCapReached

        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Unknown,
            RelativeUncapped(0.50),
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.60),
            RelativeCapNotReached {
                rel_soc: 0.50,
                cap: 0.60,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.60),
            RelativeCapNotReached {
                rel_soc: 0.50,
                cap: 0.60,
            },
        );

        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Absolute(0.45),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.45,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            SoC::Relative(0.45),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.45,
            },
        );
    }

    #[test]
    fn soc_progress_saturating_add() {
        #[track_caller]
        fn test(init: SoCProgress, added_soc: f64, res: SoCProgress) {
            assert_eq!(init.saturating_add(added_soc), res);
        }

        use SoCProgress::*;

        test(Unknown, 0.10, RelativeUncapped(0.10));
        test(Unknown, 1.10, RelativeUncapped(1.00));

        test(AbsoluteUncapped(0.50), 0.10, AbsoluteUncapped(0.60));
        test(AbsoluteUncapped(0.50), 0.60, AbsoluteUncapped(1.00));

        test(
            AbsoluteCap(0.50),
            0.10,
            RelativeCapNotReached {
                rel_soc: 0.10,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCap(0.50),
            0.60,
            RelativeCapReached {
                rel_soc: 0.60,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCap(0.50),
            1.10,
            RelativeCapReached {
                rel_soc: 1.00,
                cap: 0.50,
            },
        );

        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            0.05,
            AbsoluteCapNotReached {
                soc: 0.47,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            0.08,
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.42,
                cap: 0.50,
            },
            0.60,
            AbsoluteCapReached {
                soc: 1.00,
                cap: 0.50,
            },
        );

        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            0.10,
            AbsoluteCapReached {
                soc: 0.60,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            0.60,
            AbsoluteCapReached {
                soc: 1.00,
                cap: 0.50,
            },
        );

        test(RelativeUncapped(0.50), 0.10, RelativeUncapped(0.60));
        test(RelativeUncapped(0.50), 0.60, RelativeUncapped(1.00));

        test(
            RelativeCap(0.50),
            0.10,
            RelativeCapNotReached {
                rel_soc: 0.10,
                cap: 0.50,
            },
        );
        test(
            RelativeCap(0.50),
            0.60,
            RelativeCapReached {
                rel_soc: 0.60,
                cap: 0.50,
            },
        );
        test(
            RelativeCap(0.50),
            1.10,
            RelativeCapReached {
                rel_soc: 1.00,
                cap: 0.50,
            },
        );

        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            0.05,
            RelativeCapNotReached {
                rel_soc: 0.47,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            0.08,
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            0.60,
            RelativeCapReached {
                rel_soc: 1.00,
                cap: 0.50,
            },
        );

        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            0.01,
            RelativeCapReached {
                rel_soc: 0.51,
                cap: 0.50,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            0.60,
            RelativeCapReached {
                rel_soc: 1.00,
                cap: 0.50,
            },
        );
    }

    #[test]
    fn soc_progress_saturating_sub() {
        #[track_caller]
        fn test(init: SoCProgress, added_soc: f64, res: SoCProgress) {
            assert_eq!(init.saturating_sub(added_soc), res);
        }

        use SoCProgress::*;

        test(Unknown, 0.10, Unknown);

        test(AbsoluteUncapped(0.50), 0.10, AbsoluteUncapped(0.40));
        test(AbsoluteUncapped(0.50), 0.60, AbsoluteUncapped(0.00));

        test(AbsoluteCap(0.50), 0.10, AbsoluteCap(0.50));

        test(
            AbsoluteCapNotReached {
                soc: 0.40,
                cap: 0.50,
            },
            0.20,
            AbsoluteCapNotReached {
                soc: 0.20,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapNotReached {
                soc: 0.40,
                cap: 0.50,
            },
            0.50,
            AbsoluteCapNotReached {
                soc: 0.00,
                cap: 0.50,
            },
        );

        test(
            AbsoluteCapReached {
                soc: 0.52,
                cap: 0.50,
            },
            0.02,
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            0.05,
            AbsoluteCapNotReached {
                soc: 0.45,
                cap: 0.50,
            },
        );
        test(
            AbsoluteCapReached {
                soc: 0.50,
                cap: 0.50,
            },
            0.60,
            AbsoluteCapNotReached {
                soc: 0.00,
                cap: 0.50,
            },
        );

        test(RelativeUncapped(0.50), 0.10, RelativeUncapped(0.40));
        test(RelativeUncapped(0.50), 0.60, RelativeUncapped(0.00));

        test(RelativeCap(0.50), 0.10, RelativeCap(0.50));

        test(
            RelativeCapNotReached {
                rel_soc: 0.45,
                cap: 0.50,
            },
            0.05,
            RelativeCapNotReached {
                rel_soc: 0.40,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.52,
                cap: 0.50,
            },
            0.02,
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeCapNotReached {
                rel_soc: 0.42,
                cap: 0.50,
            },
            0.60,
            RelativeCapNotReached {
                rel_soc: 0.00,
                cap: 0.50,
            },
        );

        test(
            RelativeCapReached {
                rel_soc: 0.60,
                cap: 0.50,
            },
            0.10,
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            0.20,
            RelativeCapNotReached {
                rel_soc: 0.30,
                cap: 0.50,
            },
        );
        test(
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50,
            },
            0.60,
            RelativeCapNotReached {
                rel_soc: 0.00,
                cap: 0.50,
            },
        );
    }
}
