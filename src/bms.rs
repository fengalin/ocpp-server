use std::fmt;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Bms {
    pub capacity: f64,
    pub constant_power_loss: u16,
    pub reference_energy: Option<f64>,
    pub reference_soc: Option<f64>,
    pub soc_cap: Option<f64>,
}

impl Bms {
    pub fn set_reference(&mut self, energy: u64) {
        self.reference_energy = Some(energy as f64);
    }

    pub fn get_current_soc(&self, energy: u64) -> SocProgress {
        let Some(refrence_energy) = self.reference_energy else {
            return SocProgress::Unknown;
        };
        let added_energy = energy as f64 - refrence_energy;
        if added_energy < 0.0 {
            log::error!(
                "can't compute SoC progress: new energy: {energy}, reference energy: {refrence_energy:.0}"
            );
            return SocProgress::Unknown;
        };

        let added_soc = added_energy / self.capacity;

        if let Some(initial_soc) = self.reference_soc {
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

    /// Computes the raw energy to add, not including constant power loss.
    pub fn get_energy_to_add(&self) -> f64 {
        let soc_cap = self.soc_cap.expect("checked by args");
        let soc_to_add = soc_cap - self.reference_soc.unwrap_or_default();
        // FIXME check soc_cap > initial_soc in args & find an elegant way
        // to show it is guaranteed
        assert!(soc_cap <= 1.0);
        assert!(soc_to_add >= 0.0);

        self.capacity * soc_to_add
    }
}

#[derive(Debug, Copy, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum SocProgress {
    Unknown,
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

    pub fn soc(&self) -> Option<f64> {
        use SocProgress::*;
        Some(match self {
            Unknown => return None,
            AbsoluteUncapped(soc) => *soc,
            AbsoluteCapNotReached { soc, .. } => *soc,
            AbsoluteCapReached { soc, .. } => *soc,
            RelativeUncapped(added_soc) => *added_soc,
            RelativeCapNotReached { added_soc, .. } => *added_soc,
            RelativeCapReached { added_soc, .. } => *added_soc,
        })
    }
}

impl fmt::Display for SocProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SocProgress::*;
        match self {
            Unknown => f.write_str("unknown SoC"),
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
