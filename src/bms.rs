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
                SoCProgress::RelativeCapped(cap)
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
    AbsoluteCapNotReached { soc: f64, cap: f64 },
    AbsoluteCapReached { soc: f64, cap: f64 },
    RelativeUncapped(f64),
    RelativeCapped(f64),
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
                (None, Some(cap)) => RelativeCapped(cap),
                (None, None) => Unknown,
            }
        } else {
            match (soc, cap) {
                (Some(rel_soc), Some(cap)) if rel_soc < cap => {
                    RelativeCapNotReached { rel_soc, cap }
                }
                (Some(rel_soc), Some(cap)) => RelativeCapReached { rel_soc, cap },
                (Some(rel_soc), None) => RelativeUncapped(rel_soc),
                (None, Some(cap)) => RelativeCapped(cap),
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

    pub fn with_soc(mut self, new_soc: SoC) -> Self {
        use SoCProgress::*;
        match (&mut self, new_soc) {
            (Unknown, SoC::Unknown) => self = Unknown,
            (Unknown, SoC::Absolute(new_soc)) => self = AbsoluteUncapped(new_soc),
            (Unknown, SoC::Relative(new_soc)) => self = RelativeUncapped(new_soc),
            (AbsoluteUncapped(soc), SoC::Absolute(new_soc)) => *soc = new_soc,
            (AbsoluteUncapped(soc), SoC::Relative(new_rel_soc)) => {
                warn!(
                    "replacing absolute SoC {} with {new_soc} as per user instructions",
                    SoC::Absolute(*soc)
                );
                self = RelativeUncapped(new_rel_soc);
            }
            (AbsoluteUncapped(soc), SoC::Unknown) => {
                warn!(
                    "forgetting absolute SoC {} as per user instructions",
                    SoC::Absolute(*soc)
                );
                self = Unknown;
            }
            (AbsoluteCapNotReached { soc, cap }, SoC::Absolute(new_soc)) => {
                if new_soc >= *cap {
                    self = AbsoluteCapReached {
                        soc: new_soc,
                        cap: *cap,
                    };
                } else {
                    *soc = new_soc;
                }
            }
            (AbsoluteCapNotReached { soc, cap }, SoC::Relative(new_rel_soc)) => {
                warn!(
                    "replacing absolute SoC {} with {new_soc} as per user instructions",
                    SoC::Absolute(*soc)
                );
                if new_rel_soc >= *cap {
                    self = RelativeCapReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                } else {
                    self = RelativeCapNotReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                }
            }
            (AbsoluteCapNotReached { soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting absolute SoC {} as per user instructions",
                    SoC::Absolute(*soc)
                );
                self = RelativeUncapped(*cap);
            }
            (AbsoluteCapReached { soc, cap }, SoC::Absolute(new_soc)) => {
                if new_soc < *cap {
                    self = AbsoluteCapNotReached {
                        soc: new_soc,
                        cap: *cap,
                    };
                } else {
                    *soc = new_soc;
                }
            }
            (AbsoluteCapReached { soc, cap }, SoC::Relative(new_rel_soc)) => {
                warn!(
                    "replacing absolute SoC {} with {new_soc} as per user instructions",
                    SoC::Absolute(*soc)
                );
                if new_rel_soc < *cap {
                    self = RelativeCapNotReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                } else {
                    self = RelativeCapReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                }
            }
            (AbsoluteCapReached { soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting absolute SoC {} as per user instructions",
                    SoC::Absolute(*soc)
                );
                self = RelativeCapped(*cap);
            }
            (RelativeUncapped(_), SoC::Unknown) => self = Unknown,
            (RelativeUncapped(rel_soc), SoC::Relative(new_rel_soc)) => {
                *rel_soc = new_rel_soc;
            }
            (RelativeUncapped(rel_soc), SoC::Absolute(new_abs_soc)) => {
                warn!(
                    "replacing relative SoC {} with {new_soc} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                self = AbsoluteUncapped(new_abs_soc);
            }
            (RelativeCapped(_), SoC::Unknown) => (),
            (RelativeCapped(cap), SoC::Relative(new_rel_soc)) => {
                if new_rel_soc >= *cap {
                    self = RelativeCapReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                } else {
                    self = RelativeCapNotReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                }
            }
            (RelativeCapped(cap), SoC::Absolute(new_abs_soc)) => {
                if new_abs_soc >= *cap {
                    self = AbsoluteCapReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                } else {
                    self = AbsoluteCapNotReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                }
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Relative(new_rel_soc)) => {
                if new_rel_soc >= *cap {
                    self = RelativeCapReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                } else {
                    *rel_soc = new_rel_soc;
                }
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Absolute(new_abs_soc)) => {
                warn!(
                    "replacing relative SoC {} with {new_soc} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                if new_abs_soc >= *cap {
                    self = AbsoluteCapReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                } else {
                    self = AbsoluteCapNotReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                }
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting relative SoC {} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                self = RelativeCapped(*cap);
            }
            (RelativeCapReached { rel_soc, cap }, SoC::Relative(new_rel_soc)) => {
                if new_rel_soc < *cap {
                    self = RelativeCapNotReached {
                        rel_soc: new_rel_soc,
                        cap: *cap,
                    };
                } else {
                    *rel_soc = new_rel_soc;
                }
            }
            (RelativeCapReached { rel_soc, cap }, SoC::Absolute(new_abs_soc)) => {
                warn!(
                    "replacing relative SoC {} with {new_soc} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                if new_abs_soc < *cap {
                    self = AbsoluteCapNotReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                } else {
                    self = AbsoluteCapReached {
                        soc: new_abs_soc,
                        cap: *cap,
                    };
                }
            }
            (RelativeCapReached { rel_soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting relative SoC {} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                self = RelativeCapped(*cap);
            }
        }

        self
    }

    pub fn with_cap(mut self, new_cap: SoC) -> Self {
        use SoCProgress::*;
        match (&mut self, new_cap) {
            (Unknown, SoC::Unknown) => self = Unknown,
            (Unknown, SoC::Absolute(new_cap)) => self = RelativeCapped(new_cap),
            (Unknown, SoC::Relative(new_cap)) => self = RelativeCapped(new_cap),
            (AbsoluteUncapped(soc), SoC::Absolute(new_cap)) => {
                if *soc >= new_cap {
                    self = AbsoluteCapReached {
                        soc: *soc,
                        cap: new_cap,
                    };
                } else {
                    self = AbsoluteCapNotReached {
                        soc: *soc,
                        cap: new_cap,
                    };
                }
            }
            (AbsoluteUncapped(soc), SoC::Relative(new_rel_cap)) => {
                warn!(
                    "replacing absolute uncapped SoC {}, adding relative cap {new_cap} as per user instructions",
                    SoC::Absolute(*soc)
                );
                if *soc >= new_rel_cap {
                    self = AbsoluteCapReached {
                        soc: *soc,
                        cap: new_rel_cap,
                    };
                } else {
                    self = AbsoluteCapNotReached {
                        soc: *soc,
                        cap: new_rel_cap,
                    };
                }
            }
            (AbsoluteUncapped(_), SoC::Unknown) => (),
            (AbsoluteCapNotReached { soc, cap }, SoC::Absolute(new_abs_cap)) => {
                if new_abs_cap >= *cap {
                    self = AbsoluteCapReached {
                        soc: *soc,
                        cap: new_abs_cap,
                    };
                } else {
                    *cap = new_abs_cap;
                }
            }
            (AbsoluteCapNotReached { soc, cap }, SoC::Relative(new_rel_cap)) => {
                warn!(
                    "replacing absolute SoC cap {}, adding {new_cap} as per user instructions",
                    SoC::Absolute(*cap)
                );
                if *soc >= new_rel_cap {
                    self = AbsoluteCapReached {
                        soc: *soc,
                        cap: *cap,
                    };
                } else {
                    *cap = new_rel_cap;
                }
            }
            (AbsoluteCapNotReached { soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting absolute SoC cap {} as per user instructions",
                    SoC::Absolute(*cap)
                );
                self = AbsoluteUncapped(*soc);
            }
            (AbsoluteCapReached { soc, cap }, SoC::Absolute(new_cap)) => {
                if *soc < new_cap {
                    self = AbsoluteCapNotReached {
                        soc: *soc,
                        cap: new_cap,
                    };
                } else {
                    *cap = new_cap;
                }
            }
            (AbsoluteCapReached { soc, cap }, SoC::Relative(new_abs_cap)) => {
                warn!(
                    "replacing absolute SoC cap {} with {new_cap} as per user instructions",
                    SoC::Absolute(*cap)
                );
                if *soc < new_abs_cap {
                    self = AbsoluteCapNotReached {
                        soc: *soc,
                        cap: new_abs_cap,
                    };
                } else {
                    *cap = new_abs_cap;
                }
            }
            (AbsoluteCapReached { soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting absolute SoC cap {} as per user instructions",
                    SoC::Absolute(*cap)
                );
                self = AbsoluteUncapped(*soc);
            }
            (RelativeUncapped(_), SoC::Unknown) => (),
            (RelativeUncapped(rel_soc), SoC::Relative(new_rel_cap)) => {
                if *rel_soc >= new_rel_cap {
                    self = RelativeCapReached {
                        rel_soc: *rel_soc,
                        cap: new_rel_cap,
                    };
                } else {
                    self = RelativeCapNotReached {
                        rel_soc: *rel_soc,
                        cap: new_rel_cap,
                    };
                }
            }
            (RelativeUncapped(rel_soc), SoC::Absolute(new_abs_cap)) => {
                warn!(
                    "replacing relative uncapped SoC {} with {new_cap} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                if *rel_soc >= new_abs_cap {
                    self = RelativeCapReached {
                        rel_soc: *rel_soc,
                        cap: new_abs_cap,
                    };
                } else {
                    self = RelativeCapNotReached {
                        rel_soc: *rel_soc,
                        cap: new_abs_cap,
                    };
                }
            }
            (RelativeCapped(_), SoC::Unknown) => self = Unknown,
            (RelativeCapped(cap), SoC::Relative(new_rel_cap)) => {
                *cap = new_rel_cap;
            }
            (RelativeCapped(cap), SoC::Absolute(new_abs_cap)) => {
                warn!(
                    "replacing relative uncapped SoC cap {} with {new_cap} as per user instructions",
                    SoC::Relative(*cap)
                );
                *cap = new_abs_cap;
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Relative(new_rel_cap)) => {
                if *rel_soc >= new_rel_cap {
                    self = RelativeCapReached {
                        rel_soc: *rel_soc,
                        cap: new_rel_cap,
                    };
                } else {
                    *cap = new_rel_cap;
                }
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Absolute(new_abs_cap)) => {
                warn!(
                    "replacing relative SoC cap {} with {new_cap} as per user instructions",
                    SoC::Relative(*cap)
                );
                if *rel_soc >= new_abs_cap {
                    self = RelativeCapReached {
                        rel_soc: *rel_soc,
                        cap: new_abs_cap,
                    };
                } else {
                    *cap = new_abs_cap;
                }
            }
            (RelativeCapNotReached { rel_soc, cap }, SoC::Unknown) => {
                warn!(
                    "forgetting relative SoC cap {} as per user instructions",
                    SoC::Relative(*cap)
                );
                self = RelativeUncapped(*rel_soc);
            }
            (RelativeCapReached { rel_soc, cap }, SoC::Relative(new_rel_cap)) => {
                if *rel_soc < new_rel_cap {
                    self = RelativeCapNotReached {
                        rel_soc: *rel_soc,
                        cap: new_rel_cap,
                    };
                } else {
                    *cap = new_rel_cap;
                }
            }
            (RelativeCapReached { rel_soc, cap }, SoC::Absolute(new_abs_cap)) => {
                warn!(
                    "replacing relative SoC cap {} with {new_cap} as per user instructions",
                    SoC::Relative(*cap)
                );
                if *rel_soc < new_abs_cap {
                    self = RelativeCapNotReached {
                        rel_soc: *rel_soc,
                        cap: new_abs_cap,
                    };
                } else {
                    *cap = new_abs_cap;
                }
            }
            (RelativeCapReached { rel_soc, .. }, SoC::Unknown) => {
                warn!(
                    "forgetting relative SoC cap {} as per user instructions",
                    SoC::Relative(*rel_soc)
                );
                self = RelativeUncapped(*rel_soc);
            }
        }

        self
    }

    pub fn soc(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(soc) => SoC::Absolute(soc),
            AbsoluteCapNotReached { soc, .. } => SoC::Absolute(soc),
            AbsoluteCapReached { soc, .. } => SoC::Absolute(soc),
            RelativeUncapped(rel_soc) => SoC::Relative(rel_soc),
            RelativeCapped(_) => SoC::Unknown,
            RelativeCapNotReached { rel_soc, .. } => SoC::Relative(rel_soc),
            RelativeCapReached { rel_soc, .. } => SoC::Relative(rel_soc),
        }
    }

    pub fn cap(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(_) => SoC::Unknown,
            AbsoluteCapNotReached { cap, .. } => SoC::Absolute(cap),
            AbsoluteCapReached { cap, .. } => SoC::Absolute(cap),
            RelativeUncapped(_) => SoC::Unknown,
            RelativeCapped(cap) => SoC::Relative(cap),
            RelativeCapNotReached { cap, .. } => SoC::Relative(cap),
            RelativeCapReached { cap, .. } => SoC::Relative(cap),
        }
    }

    pub fn absolute_soc(self) -> Option<f64> {
        self.soc().absolute()
    }

    pub fn saturating_add(mut self, mut added_soc: f64) -> SoCProgress {
        let saturating_add_assign = |s: &mut f64, added_soc: f64| {
            *s += added_soc;
            if *s > 1.0 {
                *s = 1.0;
            }
        };
        match &mut self {
            SoCProgress::AbsoluteUncapped(soc) => saturating_add_assign(soc, added_soc),
            SoCProgress::AbsoluteCapNotReached { soc, cap } => {
                saturating_add_assign(soc, added_soc);
                if soc >= cap {
                    self = SoCProgress::AbsoluteCapReached {
                        soc: *soc,
                        cap: *cap,
                    }
                }
            }
            SoCProgress::AbsoluteCapReached { .. } => (),
            SoCProgress::RelativeUncapped(rel_soc) => saturating_add_assign(rel_soc, added_soc),
            SoCProgress::RelativeCapNotReached { rel_soc, cap } => {
                saturating_add_assign(rel_soc, added_soc);
                if rel_soc >= cap {
                    self = SoCProgress::RelativeCapReached {
                        rel_soc: *rel_soc,
                        cap: *cap,
                    }
                }
            }
            SoCProgress::RelativeCapReached { .. } => (),
            SoCProgress::RelativeCapped(cap) => {
                check_fix_range(&mut added_soc);
                self = if added_soc < *cap {
                    SoCProgress::RelativeCapNotReached {
                        rel_soc: added_soc,
                        cap: *cap,
                    }
                } else {
                    SoCProgress::RelativeCapReached {
                        rel_soc: added_soc,
                        cap: *cap,
                    }
                };
            }
            SoCProgress::Unknown => {
                check_fix_range(&mut added_soc);
                self = SoCProgress::RelativeUncapped(added_soc)
            }
        }

        self
    }

    pub fn saturating_sub(mut self, rhs: f64) -> SoCProgress {
        let saturating_sub_assign = |s: &mut f64, rhs: f64| {
            *s -= rhs;
            if *s > 1.0 {
                *s = 1.0;
            }
        };
        match &mut self {
            SoCProgress::AbsoluteUncapped(soc) => saturating_sub_assign(soc, rhs),
            SoCProgress::AbsoluteCapNotReached { soc, .. } => saturating_sub_assign(soc, rhs),
            SoCProgress::AbsoluteCapReached { soc, cap } => {
                saturating_sub_assign(soc, rhs);
                if soc < cap {
                    self = SoCProgress::AbsoluteCapNotReached {
                        soc: *soc,
                        cap: *cap,
                    };
                }
            }
            SoCProgress::RelativeUncapped(rel_soc) => saturating_sub_assign(rel_soc, rhs),
            SoCProgress::RelativeCapNotReached { rel_soc, .. } => {
                saturating_sub_assign(rel_soc, rhs);
            }
            SoCProgress::RelativeCapReached { rel_soc, cap } => {
                saturating_sub_assign(rel_soc, rhs);
                if rel_soc < cap {
                    self = SoCProgress::RelativeCapNotReached {
                        rel_soc: *rel_soc,
                        cap: *cap,
                    };
                }
            }
            SoCProgress::RelativeCapped(_) => (),
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
            RelativeCapped(cap) => write!(f, "capped to {}", SoC::Relative(*cap)),
            _ => {
                write!(f, "{} / {}", self.soc(), self.cap())
            }
        }
    }
}
