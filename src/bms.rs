use std::fmt;

#[derive(Copy, Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Bms {
    pub capacity: f64,
    pub initial_soc: Option<f64>,
    pub soc_cap: Option<f64>,
}

impl Bms {
    pub fn get_current_soc(&self, added_energy: u64) -> SocProgress {
        let added_soc = added_energy as f64 / self.capacity;

        if let Some(initial_soc) = self.initial_soc {
            let soc = initial_soc + added_soc;
            match self.soc_cap {
                Some(cap) => {
                    if soc >= cap {
                        SocProgress::AbsoluteCapReached { soc, cap }
                    } else {
                        SocProgress::AbsoluteCapNotReached { soc, cap }
                    }
                }
                None => SocProgress::AbsoluteUncapped(soc),
            }
        } else {
            match self.soc_cap {
                Some(cap) => {
                    if added_soc >= cap {
                        SocProgress::RelativeCapReached { added_soc, cap }
                    } else {
                        SocProgress::RelativeCapNotReached { added_soc, cap }
                    }
                }
                None => SocProgress::RelativeUncapped(added_soc),
            }
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum SocProgress {
    AbsoluteUncapped(f64),
    AbsoluteCapNotReached { soc: f64, cap: f64 },
    AbsoluteCapReached { soc: f64, cap: f64 },
    RelativeUncapped(f64),
    RelativeCapNotReached { added_soc: f64, cap: f64 },
    RelativeCapReached { added_soc: f64, cap: f64 },
}

impl SocProgress {
    pub fn is_complete(&self) -> bool {
        use SocProgress::*;
        matches!(self, AbsoluteCapReached { .. } | RelativeCapReached { .. })
    }

    pub fn soc(&self) -> f64 {
        use SocProgress::*;
        match self {
            AbsoluteUncapped(soc) => *soc,
            AbsoluteCapNotReached { soc, .. } => *soc,
            AbsoluteCapReached { soc, .. } => *soc,
            RelativeUncapped(added_soc) => *added_soc,
            RelativeCapNotReached { added_soc, .. } => *added_soc,
            RelativeCapReached { added_soc, .. } => *added_soc,
        }
    }
}

impl fmt::Display for SocProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SocProgress::*;
        match self {
            AbsoluteUncapped(soc) => write!(f, "SoC {:.1} % uncapped", 100.0 * soc),
            AbsoluteCapNotReached { soc, cap } => {
                write!(f, "SoC {:.1} % / {:.1} %", 100.0 * soc, 100.0 * cap)
            }
            AbsoluteCapReached { soc, cap } => {
                write!(f, "SoC {:.1} / {:.1} % complete", 100.0 * soc, 100.0 * cap)
            }
            RelativeUncapped(added_soc) => {
                write!(f, "added SoC {:.1} % uncapped", 100.0 * added_soc)
            }
            RelativeCapNotReached { added_soc, cap } => {
                write!(
                    f,
                    "added SoC {:.1} % / {:.1} %",
                    100.0 * added_soc,
                    100.0 * cap
                )
            }
            RelativeCapReached { added_soc, cap } => {
                write!(
                    f,
                    "added SoC {:.1} % / {:.1} % complete",
                    100.0 * added_soc,
                    100.0 * cap
                )
            }
        }
    }
}
